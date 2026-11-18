# Per-checkout indexes: lean implementation plan

## Authority and scope

Implement the architecture in [the design](per-checkout-indexes.md), amended by the operator and chair rulings r1–r5 in `.cortexkit/alfonso/rulings/per-checkout-indexes-r*.md` (later rulings win where they overlap), and by the operator's lean-plan brief. This plan replaces the old slice schedule, not the design. The amendment map at `keep/per-checkout-d0-map` is reference material for edge cases, not authority for its draft-only policies or process.

The end state is one view per checkout: immutable family-shared content artifacts, checkout manifests, and uncommitted edits in a RAM delta. Walker membership includes tracked and untracked non-ignored files in every plane. Each query uses one pinned generation/delta/intent snapshot; readiness is independent per plane and survives publication and restart. Views replace borrowing and RAM overlay, not sit alongside them as another permanent mode.

**Reporting stays on the existing response contract:** use `complete: false` and a named gap when coverage is incomplete. Callgraph-consuming queries, including inspect/dead-code, wait up to about 3 seconds for relevant work, then name the files not yet reflected. Take the answer snapshot after the wait, including on timeout; do not mix pre-wait results with a newer generation. No new freshness block, response schema, or wire-state taxonomy. Internal snapshot/readiness metadata need not become response fields.

## Schedule at a glance

Sizes are rough engineering effort including implementation and local tests, not calendar promises: M = 3–5 engineer-days, L = 6–10, XL = 10–15. Integration findings can change them.

| Slice | Depends on | Parallel with | Size |
|---|---|---|---|
| 1. Registry, blob store and view contracts | Existing baseline / related-work handoff | — | XL |
| 2. Trigram views and live edits | 1 | 3, 4, 5 construction | XL |
| 3. Semantic views, reusing shared-base overlay | 1 + semantic-overlay handoff | 2, 4, 5 construction | L |
| 4. Callgraph views and ruled method linking | 1; chair-reviewed receiver matrix before linking implementation | 2, 3, 5 construction | XL |
| 5. Sibling-seeded first load | 1 to start; 2–4 to finish production acceptance | 2, 3, 4 during construction | L |
| 6. Disk limits and LRU eviction | 2–5 | — | M |
| 7. Migration and explicit legacy pruning | 6 | — | L |
| 8. Cutover and deletion, no fallback | 1–7, bounded drill, operator fleet trial | — | L |
| 9. Multi-repo parent folders | 1, 5 | 6, 7, 8 | L |

There are three query planes and a fourth parallel workstream for seed/load orchestration. All four start after slice 1. Slice 5 can implement selection, protection and strict reconciliation against the slice-1 interfaces while planes are built, but cannot enable derived-callgraph seeding or claim end-to-end parity until slice 4 proves root independence. This is a real dependency, not a reason to serialize all plane work. Slices 6–8 deliberately serialize the shared runtime/maintenance wiring.

## Ownership and related-work handoff

Paths below are relative to `crates/aft/src/` unless prefixed otherwise. New module names are proposed concrete destinations, not claims that they already exist. Ownership includes colocated tests; each slice also owns its uniquely named `crates/aft/tests/integration/per_checkout_<slice>.rs` test file. Slice 1 registers those test modules and new production modules before the parallel wave.

`context.rs`, `runtime_drain.rs`, `commands/configure.rs`, `lib.rs`, `views/mod.rs` and shared test registration are integration hot spots. Slice 1 owns them first and installs the narrow plane/loader interfaces and routing hooks. During the parallel wave **only slice 5 owns the shared runtime files**; slices 2–4 use those hooks and own their plane modules. They request missing adapter work from slice 5 rather than editing its files. Module declarations needed by the wave belong in slice 1. Slices 6, 7 and 8 receive these files sequentially after the wave lands. Do not give two running slices the same file, even for unrelated hunks. New shared wiring is handed off, not solved with overlapping fences.

Before slice 1 establishes interfaces, compare the following related-work changes with the implementation baseline, land their accepted versions and resolve conflicts:

- Semantic overlay branch `alfonso/task/bg_71b861fe2d252049-semantic-index-shared-base-plus-per-root-deltas-`: reuse its root-relative shared base, per-root replacement/tombstone behavior and residency accounting. It changes `semantic_index.rs` and `commands/configure.rs`; do not independently build a second overlay or remove useful shared storage just because legacy terminology says `shared_base`.
- No-ready-lane branch `alfonso/task/bg_a09505d6657c6b35-aft-search-no-ready-lane-while-indexes-exist-bor`: preserve borrowed-load and warm-reload behavior until view routing replaces it. Its touched files include `search_index.rs`, `semantic_index` consumers in `commands/semantic_search/mod.rs`, `context.rs`, `commands/configure.rs`, `readonly_artifacts.rs`, and `runtime_drain.rs`.
- Preserve the RAM-overlay default before cutover; do not introduce an interim default regression. Slice 8 deletes the obsolete implementation only after replacement tests pass.

Verify that the listed related-work changes are present in the current implementation before relying on them; this document does not assert they are all merged. Existing relevant anchors include `search_index.rs`, `semantic_index.rs`, `views/{assembly,generation,read,materialization}.rs`, `callgraph_store/join.rs`, and `views/materialization/resolution_facts.rs`. Extend them rather than creating parallel stores or resolvers.

## Common proof, kept small

Slice 1 supplies one reusable parity harness; slices 2–5 supply plane-specific assertions. The oracle starts from an empty isolated store and rebuilds the frozen checkout/config from source, without reusing the tested view's blobs, seeds or derived files. Use deterministic embeddings or independently replayed model responses for semantic comparisons. Compare membership, matched lines, semantic result identities/scores, and logical callgraph rows plus bound query results—not timestamps or SQLite file bytes. Callgraph parity means cold rebuild under the **ruled** resolver, not equality with legacy name-guess edges.

Cover edits, delete/recreate, rename, revert, add-then-delete, A→B→A switches, untracked/ignored membership and ignore-file changes. Run a real process exit/restart and a separate forced-kill/reopen test wherever state persists; merely reopening an object in the same test process is insufficient. Persisted generations must be either fully installed or safely recoverable, never falsely ready. Uncommitted source bytes survive in the checkout and are strictly reconciled after restart; the RAM delta itself need not be persisted.

Keep the design's focused non-vacuity controls: intentionally break the invariant, observe its named test fail, restore, and rerun. This includes delta rebase, intent scanning, readiness after fold, membership, strict hashing, GC protection and ruled dispatch fan-out. No separate governance slices, signed records, dedicated test hosts, soak quotas or latency-percentile admission gates. Normal package checks and scoped tests belong to each implementation slice. A ranking-fence touch still follows r4: use `engine_unwired` (the ranking-preserving change protocol), run parity fixtures listed in `benchmarks/aft-search/slice-descriptors/`, and retain byte-identical ranked rows. The fence paths are defined by `RANKING_FENCE_PREFIXES` in `benchmarks/aft-search/search_quality_lib.py`; any ranking change is separate work.

## 1. Registry, blob store and view contracts

**Build:** Versioned family/view registration and immutable content-keyed blobs, canonical trigram segment storage primitives, v2 manifests (content hash, size, plane keys and Ready/Pending/Failed state), and private construction paths even for equal manifests. Define pinned snapshots, live delta/journal/intent, completion compatibility and the plane/loader interfaces used by the parallel wave. Keep producer identity in keys. Route new-store openers before the first production write; preserve identity-aware SQLite opening and backup rules.

Establish the GC handoff barrier and protection protocol now: register before work, protect before touch, verify the seed/current pointer after pinning, mark all registered/protected generations, epoch-revalidate before deletion, and fail closed on uncertainty. Protect readers, residents, live keys and assemblies, including noncurrent generations. Missing-root deregistration requires no protection of any class under the same barrier; an unreadable marker retains the member. Policy-based LRU eviction comes in slice 6.

**Owns:** `blob_store/`, `pins/`, `gc/`, `path_status/`, `views/{mod,generation,io,profile}.rs`; new `views/{registry,snapshot,readiness,contracts,parity_harness,segment_store}.rs`; the shared integration files listed above and test registrations. Introduce module hooks for the later owners without implementing their planes.

**Depends on:** Related-work handoff, not completion of the query planes.

**Proves:** Manifest roundtrip and producer mismatch rejection; file-built versus blob-built canonical segment equality; one-key conflicting-payload rejection; concurrent equal-manifest builders; rebase over `keys(delta) ∪ diff(old,new)` with journal replay and foreign CAS winner; pending/failed state surviving fold and reopen. Multi-process mark/reuse/publication races, stopped-but-live pin owners, and r4's marker-only removed-root test (two sweeps retain; reader departure and two further sweeps allow removal). Kill at blob, segment, manifest and pointer durability boundaries and restart; protected bytes remain readable. Named mutation reds for caller-only marking, removed epoch check, delta-only rebase and live-only pending counts.

**Size:** XL.

## 2. Trigram views and live edits

**Build:** Segment + generation overlay + per-checkout RAM live delta, one active source per relative path. Supersede base entries before pruning, including additions/tombstones and both sides of renames. Record write intent before acknowledgment, including formatter changes and partially failed/restored edits. Query pending intent directly before index pruning. Strictly hash after bind/rebind, watcher gaps or overflow; a delivered content-invalidating event cannot be dismissed because size/mtime match. Reconcile ignore membership. Keep direct scanning available when indexed coverage cannot be proved, with existing incompleteness/gap reporting when the scan does not cover everything.

**Owns:** `search_index.rs`, `grep_executor.rs`, `commands/{grep,glob,write,apply_patch,edit_match,edit_symbol,delete_file,delete_tree,move_file,undo,restore_checkpoint}.rs`, `write_ledger.rs`, `watcher/`, `watcher_backend/`, `watcher_filter.rs`, `gitignore_state.rs`, `walk_boundary.rs`; new `views/{trigram,live_delta,intent}.rs`. Audit the other mutation entrypoints and assign any needed additional mutator files exclusively to this owner before editing; no runtime shared-file edits.

**Depends on:** 1.

**Proves:** Parity harness compares matched lines and membership after every switch/edit schedule, including withheld watcher events immediately after successful and partial-error AFT writes, rollback restoration, healthy preserved-mtime rewrites and overflow. No phantom rows before any generic on-disk presence guard. Restart/kill during segment publication and switch, then strict reconciliation equals cold rebuild. Mutation reds for missing intent scan, missing overlay supersession, missing membership filtering and stat-only overflow recovery. Count memory with shared segment bytes once and live checkout metadata separately (representative 10/40-view fixtures, not a dedicated-host gate).

**Size:** XL.

## 3. Semantic views, reusing the shared-base overlay

**Build:** Adapt the already-built shared-base/per-root-delta implementation to view keys and manifest membership. Reuse base vectors; replacements and tombstones are local. Populate the resident arena at admission, score only compatible current members, and keep SQLite decoding out of scoring. Share identical content/producer work across views and fill each checkout within its budget. Pending/failed entries survive fold, eviction and restart; stale content/model/chunker/template completions cannot resurrect old vectors. Include current membership checks even when semantic search is the first query after an ignore change.

**Owns:** `semantic_index.rs`, `commands/semantic_search/`, `refresh/`, `views/semantic_fill_tests.rs`; new `views/{semantic,semantic_arena}.rs`. Configure/runtime adapters belong to slice 5. Ranking descriptors/fixtures needed by this slice are exclusively this slice's during the wave; trigram or loader work needing the same descriptor is handed off rather than edited concurrently.

**Depends on:** 1 and the accepted semantic-overlay implementation.

**Proves:** Semantic parity against full rebuild for unchanged and changed roots, model changes, ignored/reincluded files and tombstones; embed calls equal compatible misses, two views/one key produce one model call. No query-path SQLite vector decoding, shared payload counted once, private growth proportional to edits, cleanup after unbind. Real restart and kill during fill/publish preserve missing/failed coverage and reject late incompatible completions; cold results match when ready. Keep the overlay's existing shared-memory/borrowed-load regression tests until cutover replaces their transport assumptions. Mutation red for losing pending work after fold and for scoring superseded base vectors.

**Size:** L.

## 4. Callgraph views and ruled method linking

**Build:** Hints-only joining from immutable extraction blobs, incremental resolution invalidation, derived graph per view, and root-independent materialization. No checkout source reads in the join. Use the r2 receiver-form table exactly; unsupported receiver forms are unknown, not inferred opportunistically. Known concrete receivers link exact definitions plus applicable subtype overrides; inherited calls link the nearest ancestor plus overrides; interface/trait/abstract receivers link the declaration and every project implementation. Final/sealed and Rust/Go concrete values use only the concrete method. Label dispatch edges as possible targets; every target is live. Builtin/library methods never link to same-named project methods.

Unknown `x.m()` keeps every same-language project method with source name `m` live, without arity/visibility/type/module filtering, but creates no callers/impact/trace edge. Candidate-bearing sites count unresolved; written-name sites with no candidates count external. Syntactic dynamic access has no candidates and is counted separately only in JS/TS and Python; Rust/Go/Java/C#/Kotlin have explicit zero dynamic rows, not reflection substitutes (r4).

**Owns:** `callgraph_store/`, `callgraph.rs`, `callgraph_maintenance.rs`, `calls.rs`, `extract.rs`, `views/assembly.rs`, `views/materialization.rs`, `views/materialization/`, `views/dispatch_parity_probe.rs`, `commands/{call_tree,callers,callgraph_store_adapter,impact,trace_data,trace_to,trace_to_symbol}.rs`, `inspect/`, `commands/inspect.rs`; new `views/callgraph.rs`; `docs/design/per-checkout-dispatch-matrix.md`. Shared runtime wait routing belongs to slice 5.

**Depends on:** 1. First transcribe r2–r4's receiver forms × concrete/inherited/interface 0/1/2/external/unknown matrix; chair reviews it before the linking implementation begins. This is a checkpoint within the slice, not a separate implementation campaign.

**Proves:** All matrix cells assert edge provenance and liveness; preserve the combined public unknown + dynamic fixture in Python, JS and TS (unresolved/dynamic/external = 1/1/0, no edges), and overload/private cells where expressible. Retain static-call behavior outside the receiver table. Remove legacy name-guess parity as an oracle. Two real roots with different parent/nonmember config produce equal logical tables and bound answers with roots unreadable during join; seeded incremental equals cold including dispatch relinking. Reuse existing `views::materialization::tests`, manifest-join integration, live-WAL backup and cold-fallback tests. Real restart and forced kill during derived publication recover cleanly. Mutations removing interface fan-out, adding arity/visibility filtering and embedding a root in a row must redden the corresponding named tests.

**Size:** XL.

## 5. Sibling-seeded first load and runtime integration

**Build:** Select/pin a compatible family seed, strictly reconcile the new checkout, serve seed plus local delta, then install its own generation. A seed is an optimization, not borrowed ownership or permission to score another root's excluded members. Verify seed pointer after pinning; handle sibling publication/GC and local edits during load. No-git fallback is a full content walk. Enable derived-callgraph seeding only after slice 4's final-materializer proof.

This owner integrates the four workstreams through slice-1 hooks, including warm reload, all callgraph-consuming waits (about 3 seconds total, not repeatedly reset by new edits), final pinned snapshots, and existing named-gap reporting. Incomplete semantic/callgraph readiness must not disappear because lexical search succeeds. Implement read-only foreign-view access with reader markers and existing incomplete/gap reporting; no foreign blob writes, builds, repairs or publication. Preserve first-party write capabilities across primary roots, linked worktrees, shared/plain clones and copied trees before removing borrower-only restrictions. Keep unverified/federated readers read-only.

**Owns:** `context.rs`, `runtime_drain.rs`, `runtime_registry.rs`, `commands/configure.rs`, `readonly_artifacts.rs`, `views/read.rs`, `cold_build_limiter.rs`, `response_finalize.rs`, plus shared `lib.rs`, `views/mod.rs` and test registration if an adapter needs them; new `views/{first_load,query_wait}.rs`. It alone changes shared runtime wiring during the parallel wave.

**Depends on:** 1 for construction; 2–4 for end-to-end acceptance and enabling production seed paths.

**Proves:** Each plane's seed-plus-delta results equal an independent rebuild for all five checkout kinds; no-ready-lane and warm-reload regressions stay green. Race seed switch/sweep against load, add edits during reconciliation, and test foreign read-only access by a filesystem-write audit excluding markers. Query waits cover successful installation and timeout: after timeout `complete: false` and actual unreflected filenames, not generic degradation; a completed wait uses installed data, not just a published pointer. Restart and forced kill before/after own-generation install preserve seed safety and reconcile to cold results. Record bounded load-time/memory observations against the design estimates, separately from embedding/extraction cost; do not invent performance pass thresholds.

**Size:** L.

## 6. Disk limits and LRU eviction

**Build:** Apply r1's 4 GiB soft / 6 GiB hard per-repo view-store disk limits, replacing the undecided derived-store policy in the design, with LRU eviction favoring checkouts without an active session. Never evict an active session's store or protected generation to hit a number. Queue/refuse new protected work honestly when protected bytes prevent admission. Retain the design's separate shared-family/segment and resident-memory accounting; do not silently reinterpret all resource domains as one limit. Extend slice-1 GC to reclaim eligible unreferenced blobs/segments and physically reclaim database space. Segment deletion/recreation must not unlink a newly adopted segment after lock handoff.

**Owns, after the wave:** `gc/`, `pins/`, `blob_store/`, `views/{registry,segment_store,profile}.rs`, `cold_build_limiter.rs`, `context.rs`, `runtime_drain.rs`; new `views/eviction.rs` and resource tests. No concurrent owners of these files.

**Depends on:** 2–5.

**Proves:** Deterministic small-budget tests exercise actual accounting and eviction/admission, not mocked totals: LRU order, active sessions, pinned old generations, assemblies, unreadable protection, all-protected overage and rebind after eviction. Rebuild/rehydrate after eviction matches every plane's cold oracle. Actual bytes shrink; shared keys count once. Real process restart and kill during sweep/segment delete-recreate retain protected data and finish safely. Include r4 missing-root marker-only regression. Report representative occupancy at configured defaults without requiring a special host or a physical multi-GiB exhaustion trial.

**Size:** M.

## 7. Migration and explicit legacy pruning

**Build:** Durable versioned import with single flight, idempotent transitions and canonical conversion; incompatible artifacts rebuild instead of being relabeled. Keep old sets for offline rollback; no automatic legacy-set deletion at startup, sweep or maintenance. Leave the existing seven-day orphan-root sweep unchanged (r4). Implement `aft cache prune-legacy [--yes]`, dry-run by default, listing whole sets and bytes and refusing on a live holder, census error or unclassifiable live process.

Help and dry-run output must contain r5's operator contract: “Stop every AFT process (the daemon, OpenCode and Pi hosts) before running this. The check below refuses when it sees one, but it can't stop a process that starts after the check.” After a clean census, atomically rename each entire legacy set within the filesystem into `<storage>/.prune-legacy-<timestamp>/`; re-census before deleting. If a process appeared, retain/report moved path and bytes and exit nonzero. A later clean run may remove leftovers. Refused/non-atomic rename means skip/report, never delete in place. The census narrows an accepted race; it is not process exclusion.

**Owns:** `migration/`, `legacy_partitions.rs`, `cli/`, `main.rs`, `views/io.rs`, `context.rs`, `commands/configure.rs`; new `migration/per_checkout.rs` and `cli/prune_legacy.rs`, plus only the CLI package adapter files actually required by command routing, serially after the disk-limit and LRU-eviction work in section 6.

**Depends on:** 6; full migration gate completes before trial/cutover.

**Proves:** Every import transition, real kill/restart at each durable boundary, repeated import no-op, concurrent first binds perform one import, old views-on binary coexistence and old sweep isolation, incompatible cache rebuild, and offline rollback with retained legacy data. Migrated planes equal cold rebuild. In isolated `AFT_STORAGE_DIR`, positive prune removes the whole legacy inventory while v2/unrelated families remain byte-identical; negatives retain all bytes. Deterministically simulate opener between census/rename and process arrival between rename/delete; check absent old path/intact moved inventory, nonzero refusal and later clean leftover removal. Test atomic-rename refusal and help/dry-run warning. A universal refusal cannot pass the positive test.

**Size:** L.

## Before slice 8: two bounded checks, not a soak program

Run the existing branch-switch drill (`scripts/views-branch-drill.sh`) on this repo and opencode against the integrated views-on candidate after slices 1–7. Fix a short repeatable sequence of switches, uncommitted edits/reverts, untracked/ignore changes, restart and a forced-kill recovery; compare all three planes with the independent cold oracle after quiescence and check named gaps during pending work. Capture failures and fixes in ordinary test output. Include representative checkout-kind fixtures in the automated suite; no hours-per-kind, switch-count quotas, signed trial records or dedicated hosts.

Then the operator runs a **views-on trial on the real fleet**, after the r1 prerequisite that the 0.58.0 daemon is deployed on that fleet. The operator decides when the trial is sufficient and whether to proceed; this plan invents neither a duration nor success thresholds. It is a prerequisite to deleting the old paths, not a post-cutover observation. Repeat affected drill cases after fixes.

## 8. Cutover and deletion, no fallback

**Build:** Make views the sole runtime in 0.60 and delete RAM overlay and index borrowing, including borrower resolution/gates, borrowed reconciliation, bound-root legacy callgraph reads and per-root legacy index publication. Retain the useful semantic shared immutable base/delta machinery from slice 3, removing its borrower-specific API/ownership model rather than duplicating it. Preserve the generic on-disk presence guard for its independent purpose, but prove view membership works before that guard. No `views.enabled=false` fallback and no parallel legacy search lane.

Per r1, accept and ignore `worktree.ram_overlay` and `views.enabled` with deprecation notices in 0.60; reject them in 0.61. Delete runtime behavior now; keep only the small versioned parser compatibility and bounded migration readers needed for offline import/rollback. Removing those readers after the support window is later maintenance, not a ninth prerequisite or runtime fallback.

**Owns:** Sequential handoff of the runtime, search, semantic, view-reader and configure files above; `config.rs`, `config_resolve.rs`, `config_fix.rs` and their tests; applicable config/notice/CLI adapters in `packages/{aft-bridge,opencode-plugin,pi-plugin,aft-cli}/`; existing branch-drill scripts/tests and ranking descriptors only as needed. No implementation slice runs concurrently with this deletion pass.

**Depends on:** 1–7 plus the bounded drill and operator fleet trial.

**Proves:** Full integrated parity/restart/crash cases through default public entrypoints, all five checkout kinds, no-ready-lane/phantom regressions, retired-key ignored-with-notice and next-version rejection, and no execution of old borrowing/overlay behavior even under error or unavailable-index conditions. Check config parity and TypeScript typechecks for touched adapters. Review remaining references by behavior, not just symbol-name grep; migration-only readers cannot be reachable from live queries. Ranking rows stay unchanged. Offline rollback remains previous binary plus retained legacy caches, never a hidden runtime fallback.

**Size:** L.

## 9. Multi-repo parent folders

**Build:** A session opened in a folder that isn't a git repository but contains repositories (up to the repo cap) builds no index of its own. It queries its child repositories' registered views through the family registry, read-only, loaded once and kept warm, and never owns or builds them. A child is current when its files are unchanged, as views already determine. Grep, glob, `aft_search`, the call graph and inspect fan out across the children and merge their answers; results outside any child are reported as a named gap. The parent session is a registry reader that is not a checkout: it registers a reader marker, not a view, and protects the child generations it serves with read markers under slice 1's protection protocol.

This replaces the measured parent-folder attempt (branch `umbrella/integration`; the rebase-and-measure report is on `alfonso/task/bg_8e84c81f76cfcbb2-umbrella-sessions-rebase-onto-main-and-measure-o`), whose failures all came from loading other checkouts' indexes on demand inside each call.

The repo cap (today 32; the operator's own folder has 39) is decided when this slice is specified, with a recommendation to raise it or waive it for folders listed in config. Home folders keep today's behaviour.

**Depends on:** 1 and 5. It can run in parallel with 6–8.

**Size:** L.

## Decisions

1. **Distinct private source names in the required receiver fixture: A.** Matching stays on the source name as written (Python `__m` matches only `__m`), and distinct `__m`/`#m` private methods are tested separately, keeping same-name visibility cells where expressible. Slice 4 still obtains the chair's matrix clarification for the disputed cells before implementing them; the combined public+dynamic fixture is not changed silently.
2. **Semantic-required search of an external root with no view: B.** Keep the existing no-ready-semantic behaviour and its named gap. No bounded lexical scan and no new lexical ranking behaviour is added. Slice 5 verifies this against the no-ready-lane handoff when it implements that branch; no new response schema is needed.
3. **Eligibility for explicit legacy pruning: A.** `aft cache prune-legacy` prunes a legacy set only after its import has completed and the 14-day retention has elapsed, even with `--yes`. No automatic pruning, and the clean-census and rename-aside safeguards still apply.
