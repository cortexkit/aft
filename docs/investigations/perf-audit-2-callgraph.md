# Callgraph performance audit: checked claims and output equivalence

## Scope and result

The input is the Callgraph section of the static performance audit, plus its
cross-references to the callgraph implementation. The checked base is AFT
`54a3d93e230e23e580c40a0164d5cfe2bdc9561d`. Three work reductions are implemented:

* Manifest directory probes seek to their bytewise prefix rather than starting
  at the beginning of the manifest.
* Fresh staging checks the stored primary key before reading and hashing source
  bytes that extraction will read anyway. Resumed committed files still have
  their content checked.
* One immutable dispatch emission pass memoizes target selection by receiver,
  member and scope. Unknown receivers share answers by language; typed receivers
  remain caller-file scoped because imports affect type resolution. Caller,
  location and ordinal still control row emission, not target selection. Answers
  use shared ownership, and the memo retains at most 4,096 keys without clearing
  the existing hot set when full.

The last reduction removes repeated scans, **not** scans for every distinct
receiver query. That residual part of the resolver claim remains deferred.
No schema, producer or build-output version changes are made. In particular,
`BUILD_OUTPUT_VERSION` remains `v13-typed-dispatch-precision`.

This is not a claim that every static finding has been optimized. The census
below distinguishes measured reductions, prior defenses, and confirmed work
left unchanged. Deferred rows were inspected but not independently instrumented;
their counts are explicitly unknown rather than inferred from wall time.

## Work counts and selective red controls

Counts are actual visited manifest members, visited resolver file candidates,
source reads at the staging freshness seam, and SQLite WAL frames/checkpoint
pages. Fixtures exercise 4,096 manifest entries, 128 TypeScript class files with
192 dispatch sites, and a 98-source workspace with 96 importers. The WAL fixture
has up to 256 files, 16,384 functions and a 47,124,480-byte database.

| Guard | Before | After | Output assertion |
| --- | ---: | ---: | --- |
| `perf_audit2_manifest_directory_work_is_local_to_the_prefix` | 20,436 member visits | 21 | directory membership, missing directory, canonical path and 16 children |
| `perf_audit2_dispatch_repeated_sites_do_not_rescan_the_project` | 36,864 file visits | 384 | every site's complete resolution equals the uncached resolver, including protected members |
| `perf_audit2_fresh_staging_does_not_read_sources_before_extraction` | 98 staging reads | 0 | 98 extraction parses and at least 192 stored edges |
| `perf_audit2_one_function_refresh_writes_a_delta_not_the_store` | current base already has delta writes | same delta writes | every stored graph row equals an independent cold build after the edit |

All three new-fix guards failed before their fixes: 0 passed, 3 failed, 1
ignored. The ignored test is the explicit real-repository probe, not a skipped
counting guard. After adding the WAL guard, the counting filter passed 4 tests,
with only that corpus probe ignored.

Each independent mutation started from the staged live state, captured a
non-empty unstaged diff, ran the complete four-guard filter, and restored with
`git checkout -- <path>` followed by `touch <path>`. Every restore had an empty
`git diff --stat`. Mutations carried the marker `NON-VACUITY BREAK` and none was
committed.

| Reverted defense | Sole failing guard | Failure | Unstaged diff during / after restore |
| --- | --- | --- | --- |
| Read/hash before the staged PK lookup | `perf_audit2_fresh_staging_does_not_read_sources_before_extraction` | `left: 98; right: 0` | `mod.rs`: 7 insertions, 11 deletions / empty |
| Start directory probes at the manifest beginning | `perf_audit2_manifest_directory_work_is_local_to_the_prefix` | `directory probes must not scan unrelated members: 20436` | `facts.rs`: 5 insertions, 5 deletions / empty |
| Ignore dispatch memo hits | `perf_audit2_dispatch_repeated_sites_do_not_rescan_the_project` | `repeated receiver/member queries must be resolved once: 36864` | `dispatch.rs`: 2 insertions, 1 deletion / empty |
| Bypass existing equal-row UPSERT predicates for nodes, hints and refs | `perf_audit2_one_function_refresh_writes_a_delta_not_the_store` | `one-function edit must not rewrite unchanged rows or full tables: [(158, 277), (302, 277)]` | `mod.rs`: 4 insertions, 3 deletions / empty |

For **each** mutation, the other three guards in the first table passed: 3
passed, 1 failed, 1 ignored. The WAL mutation is a control for a prior fix, not a
new product change in this delivery. Full failing outputs are retained in
`tmp/perf-callgraph/mutation-{staging,manifest,dispatch,refresh-delta}.log`.

## Incremental write-ledger follow-up

The reported production ledger totals (13.9 GB refresh and 5.4 GB checkpoint in
ten hours for prefrontal; 180 GB and 106 GB over all time) do not, by themselves,
identify the size of one refresh or the running executable's revision.

The new fixture changes one function body at the end of a file by **adding a
call edge**, not just changing a comment. It keeps other symbols' byte positions
unchanged. After a truncate checkpoint establishes an empty WAL, it records
`total_changes`, WAL bytes, and the frames checkpointed by a passive checkpoint.
No concurrent reader pins the WAL. The checkpoint payload is checkpointed pages
times page size, not a process-wide physical-write estimate. A cold rebuild of
the edited fixture must have exactly the same stored graph rows.

| Configuration | DB bytes | Logical changed rows | WAL bytes | Dirtied frames | Main-DB checkpoint payload |
| --- | ---: | ---: | ---: | ---: | ---: |
| Current base/head defense, 32 files | 6,025,216 | 29 | 379,072 | 92 | 376,832 |
| Current base/head defense, 256 files | 47,124,480 | 29 | 370,832 | 90 | 368,640 |
| Equal-row defense neutralized, 32 files | 6,025,216 | 277 | 650,992 | 158 | 647,168 |
| Equal-row defense neutralized, 256 files | 47,124,480 | 277 | 1,244,272 | 302 | 1,236,992 |

The current implementation did **not** reproduce store-sized rewriting: 8x the
membership had slightly less WAL/checkpoint volume. Neutralizing the existing
equal-row predicates reproduced both unchanged-row rewriting and increased
volume as the store grew, and only the named WAL guard failed. Indexed rows
spread over more B-tree pages in the larger store, so even a file-local rewrite
can increase bytes with store size without a full-table rewrite.

Current refresh uses conflict predicates and stale-ID deltas
(`mod.rs:14356`, `14707`, `14843`, `15218`). It does not execute VACUUM, REINDEX,
or rebuild secondary indexes. Idle checkpointing is already throttled to once
per root per 60 seconds (`2508`, `2557`), and the writer's autocheckpoint is
4,000 pages, not one full checkpoint per refresh (`9383`). This agrees with the
prior write-amplification investigation. This finding is therefore **not
reproducible as stated on the checked revision**, with an existing defense
independently proved. A production-store copy, running revision and exact edit
sequence are still needed to explain the ledger totals. No speculative change
to durability, checkpoint cadence, or positional identities was made.

## Exact real-repository equivalence

The method reuses the extraction-work investigation's frozen tracked regular
files and independent before/after artifacts, extended through persisted graph
construction and the dead-code consumer. AFT inputs were archived from the base
before editing; fd was cloned into this worktree at
`9fe5cb7860163d6ed8889d014d8e00aab7e05bd5`. Resolution configs and ignore files are
included. No parent checkout or live store is used.

`perf_audit2_real_repo_output_equivalence` builds both the mutable legacy store
and an immutable manifest-derived view. It dumps every sorted legacy graph row,
every typed view table row plus schema, and the actual dead-code scanner's
aggregate and file contributions. Dead-code projection rows are also included.
Only ephemeral fixture roots, the projection's generation timestamp, and the
view's operational `meta` table are excluded. No edge, dispatch, caller,
protection, liveness, dependency, or dead-code field is normalized. Comparisons
use exact bytes from distinct before and after files, not just hashes.

| Corpus | Source files | Legacy edges | View edges | View dispatch sites | Staging reads before / after |
| --- | ---: | ---: | ---: | ---: | ---: |
| AFT | 3,247 | 120,770 | 133,924 | 238,845 | 3,247 / 0 |
| fd | 38 | 857 | 308 | 1,440 | 38 / 0 |

Each before and after probe passed one test. Both storage paths' edge and
dead-code artifacts compare exactly for each corpus:

| Corpus / artifact | Bytes | SHA-256 (both builds) |
| --- | ---: | --- |
| AFT legacy graph rows | 338,672,634 | `f7c445e77b3be35a6442fbcdc555bff68723a032f85dc0bb870c147c72de0ae0` |
| AFT legacy dead code | 133,377,008 | `b05abce1a058ffab8b3ef5e1c2d77d108aa2812c817ed8309d1267e007ff9d61` |
| AFT view rows | 560,116,941 | `4a74446c015732fd7d5aa9ecf75311e9d17dfd01ffa60395c68a602b80312677` |
| AFT view dead code | 126,355,739 | `5b8bd4dafb5b69b88c9d463efb50997eb895a3a7fe2f442e69c30f1171ac7247` |
| fd legacy graph rows | 1,906,692 | `466e7e58a680c4a3521be7de699f74657f688a31d2b9b8bc75b48017ea9462cb` |
| fd legacy dead code | 654,131 | `18fe7d9ff1ebb2f5f1269d75ff0d5991333ac623adf6ba7131c541b5169afc9d` |
| fd view rows | 2,507,327 | `1345d4d50ed46274cb7c06c2d60ba41a1d7628a4e71206f7f17af6cc84422fa0` |
| fd view dead code | 589,076 | `1fa3ec1b2fd523532b009d1f557c927495ff24b4f1458ca1a31f9af4333964de` |

The full AFT probes took 4,965 and 3,346 seconds on the loaded host. These are
not performance claims or counting-test inputs. The large corpus was used only
for the required equivalence pair; iteration and mutations use fixed fixtures.

## Finding census

Numbers are audit line numbers, not current source line numbers. Current source
locations below describe this delivery. `Unknown` means not independently
counted; deferred findings are not silently being marked solved. Repeated
summary entries at audit lines 49, 50, 64 and 65 refer to the same claims.

| Finding | Verdict and current evidence | Before / after work counts |
| --- | --- | --- |
| 430: legacy dispatch receiver inference reparses source | **Deferred.** `mod.rs:15096`, `15744` still own a chunk-local source/tree cache. The one-parse extraction change does not remove this legacy inference path. Reusing extraction trees needs a bounded lifetime across the publication pass and declaration-file lookups. | Unknown; unchanged. Store parse counters do not include this separate parser, so their 1/file result is not evidence for this claim. |
| 431: ruled resolver scans files/methods per site | **Fixed for repeated queries; deferred for distinct queries.** `dispatch.rs:1148`, `1291`, `1371`, `1480` still scan on cache misses. `MemoizedResolver` freezes input ownership and removes repeat scans without changing resolution rules. Indexing distinct type/method/hierarchy queries is separate work. | 36,864 / 384 file visits for 192 sites; full-corpus output exact. |
| 432: manifest `is_dir`/`list_dir` scans | **Fixed.** `facts.rs:83`, `160` use `Manifest::entries_from`; canonical component checks inherit the reduction. | 20,436 / 21 member visits for 4,096 members. |
| 433: borrowed response loads identities and performs Git work | **Deferred.** `callgraph_borrowed.rs:389-405`, `512-540`, `564-579` still copy identities, load graph identities and optionally spawn Git. Caching needs keys for graph revision, checkout/owner HEAD and watcher overlays; a stale disclosure is user-visible. | Unknown; unchanged. |
| 434: Rust identifier parent/sibling walks | **Deferred.** `calls.rs:557`, `568`, `604`, `648-657` still traverse ancestry and previous siblings. Replacing lexical shadowing with a scope walk requires callback/pattern parity beyond parse-count evidence. | Unknown; unchanged. |
| 435: cold staging reads before a never-hit PK lookup | **Fixed.** `mod.rs:9936` checks PK first; resumed rows still read/hash. | Fixture 98 / 0 reads; AFT 3,247 / 0; fd 38 / 0. |
| 436: per watcher-batch connection/schema setup | **Deferred.** `mod.rs:2560`, `4814-4831` retain opener/schema/root repair. Changing connection ownership crosses leases, generations and publication; existing prepared-statement reuse already bounds compilations but does not eliminate opens. | Current fixture: refresh 10 files=126 compiled statements, 40 files=126; open counts unknown. |
| 437: serialized row comparisons and repeated ref resolution | **Deferred.** `mod.rs:17489-17753` still constructs JSON comparisons and resolves refs for equality, followed by delta resolution. Typed row equality is possible but needs independent mixed-provenance/position coverage. | Unknown; unchanged. |
| 438: selected dependents fully reparsed | **Deferred for selected dependents; prior selection defense confirmed.** `mod.rs:5835-5853` reparses only selected callers, but still parses each selected file even for few refs. Avoiding those parses needs retained binding inputs rather than stale source assumptions. | Current TS addition=3 parses; Rust addition=2, independent of unrelated importers. |
| 439: unresolved qualified-ref scan on refresh | **Deferred.** `mod.rs:15393` still scans qualified unresolved refs when non-JS candidate names change. Persisting a suffix identity would affect stored evidence/versioning; query alternatives need measured plan/output parity. | Unknown; unchanged. |
| 440: candidate word/suffix allocations | **Deferred.** `mod.rs:17071-17073`, `17119` still format suffixes and split each scored candidate. A candidate-derived cache must preserve exact Unicode and tie behavior. | Unknown; unchanged. |
| 441: incremental inline target path clones/sorts and parent scans | **Deferred.** `mod.rs:3623`, `3723-3724`, `14069-14082` retain in-memory path cloning/sorting after one lazy table load. | Lazy SQL scans already bounded; clone/iteration counts unknown. |
| 442: disk inline target scans every Rust file per call | **Deferred residual scan; prior SQL defense confirmed.** `mod.rs:3932`, `4072-4079` reuse one path-ordered list but still iterate it. A logical module-prefix index must preserve caller-first and path-order selection. | Current 20 / 80 call fixtures both execute 2 lookup scans and compile 275 statements; memory iterations unknown. |
| 443: import-set clones per ref | **Deferred.** `mod.rs:11910` still clones the file's import-dependency set per reference; staged reconstruction and resolution also clone dependencies at `10132`, `12729`. Sharing lifetimes across staged windows and incremental extracts needs separate memory/copy counters. | Unknown; unchanged. |
| 444: unused persisted AST-node payload | **Deferred.** `join.rs:181`, `229` still persist AST nodes. Dropping serialized data would change blob bytes and needs an explicit compatibility decision; no producer/output bump is made here. | Unknown serialized-cost count; unchanged layout. |
| 445: dispatch type targets load/rebind all blobs | **Deferred.** `join.rs:1437-1451` still loads payloads and binds complete type-target inputs before dispatch. Sharing the ordinary join's bound index requires a larger materialization seam change. | Unknown; unchanged. |
| 446: per-import fresh module memo in view binding | **Deferred.** `join.rs:1138` calls `mod.rs:19362-19374`, which creates a fresh memo. Reusing a memo must replay config-consultation provenance for each consumer. | Unknown; unchanged. |
| 447: ancestor walks, pnpm reads, repeated realpath probes | **Deferred residual work; existing package memo confirmed.** `callgraph.rs:2760`, `2842`, `2861`, `3066`, `disk_facts.rs:65` remain. Consolidating probes must retain missing-config facts and filesystem identity semantics. | Current workspace: 24 importers=27 package reads; 96=31 (parallel-read slack 32); ancestor/probe counts unknown. |
| 448: trace queue scans and path clones | **Deferred.** Adapter `1079`, `1115` still scan pending endpoints and clone paths, although expansion/queue size and caller loads are already bounded. Parent-linked path storage must preserve path order, cycle guards and lower-bound counts. | Existing convergent fixture: 15 expansions, 7 distinct caller targets; clone counts unknown. |
| 449: call-tree N+1 loads | **Deferred residual per-unique-node work.** Adapter `2587`, `2606` memoizes adjacency but still fetches adjacency and resolves symbols per unique node. A full frontier rewrite can alter path-local budgeting/order. | Existing convergent fixture: 1,202 uncached / 406 memoized forward queries; rendered bytes equal. |
| 450: overflow reconciliation hashes size-matching files | **Deferred deliberately.** `mod.rs:7493` still hashes after size comparison. After lost events, mtime alone cannot prove unchanged bytes; skipping hashes risks stale graph output. | Unknown bytes; unchanged safety behavior. |
| 451: store-level trace path expansion | **Deferred.** `mod.rs:6586-6618` still expands paths without the adapter's expansion budget. Adding an output cap would change this trait/benchmark API's result; memo-only work needs its own path-count fixture. | Unknown; unchanged. |
| 452: decoded blobs retained across join | **Deferred.** `join.rs:2006-2015` keeps an owned decoded map through the join. Reducing retention while keeping borrowed resolver inputs alive is a memory-lifetime refactor, not a parse-count fix. | Unknown peak retention; unchanged. |
| 453: serialized resolver-surface digests per lookup | **Deferred.** `join.rs:2402`, `2477` still serialize/hash surface answers. Those persisted consultation digests are invalidation evidence; a replacement requires exact digest parity. | Unknown; unchanged. |
| 454: no backend status index | **Deferred.** `mod.rs:9601` indexes `(file_path, backend)` while `17335` filters root/status; no status index was found. Index addition has persistent write amplification and setup/version costs; needs read/write plan census first. | Unknown; unchanged. |
| 455: dynamic created-file GLOB queries and tsconfig scan | **Deferred.** `mod.rs:18462-18466`, `18535` remain; one shared memo per created-file batch is already present. Indexing candidate specifiers must retain missing-member invalidation. | Unknown; unchanged. |
| 456: pruned staging deletes while indexes dropped | **Deferred residual pruning.** `mod.rs:9942` already skips deletes for absent PKs on fresh staging, but genuinely pruned committed rows still pay deletion work. Bulk pruning needs a crash-resume fixture and scan counters. | Fresh absent-path delete defense present; prune scan counts unknown. |
| 457: two DELETEs per ref ID | **Deferred.** `mod.rs:19106-19112` still issues cached edge/ref deletes per stale ID. Batched placeholders must obey SQLite limits and preserve exact ownership of edges. | Two executions per stale ref; fixture count unknown. |
| 458: snapshot canonicalizes each stored path | **Already fixed for repeated paths.** `dead_code_projection.rs:965-970` caches by stored path; one canonicalization per distinct path remains necessary for identity. | Repeated-path probes use the map; independent snapshot canonicalization count not collected here. |
| 459: parser creation per macro token tree | **Deferred.** `calls.rs:966-971` still constructs a parser for each fragment; necessary macro-body parsing is not the same as repeated whole-file parsing. Pooling recursive fragment parsers needs range/reentrancy proof. | Existing item-macro guard: exactly 2 distinct range parses; parser construction count unknown. |
| 460: trace-data read/parse per hop | **Deferred.** Adapter `1435-1437` intentionally reads current disk while using stored cross-hop edges. A per-response source/tree cache needs current-source freshness and shared-tree lifetime tests. | Unknown; unchanged. |
| 461: deep `FileExtract` clone | **Deferred.** `mod.rs:5861` still clones changed extracts into caller extracts. Removing it needs shared ownership across changed/touched maps and cannot be treated as dead data. | One clone per changed caller; copied-byte count unknown. |
| 462: redundant second node lookup | **Already fixed on the single-node outgoing path.** `mod.rs:8057-8086` joins the target node once and does not look it up again. Batched frontier loading still separately chooses logical representatives (`7975`), which is intentional positional-symbol semantics. | One joined target fetch on the single-node path; independent query count not collected here. |
| 463: full memo clearing at capacity | **Deferred.** `mod.rs:3801-3808` clears the disk file-index memo while retaining the active caller. An LRU policy needs eviction-work and peak-memory counters. | Unknown; unchanged. |
| 464: per-node caller counts | **Already fixed in the adapter boundary path.** Adapter `2535` fetches a whole boundary in one count call. | Existing boundary fixture: 1 count query for 21 repeated sites; exact serialized callers/impact assertions pass. |
| 465: COUNT files per response | **Deferred.** Adapter `666` still obtains scanned-file count. A cached count must invalidate on file creation/deletion and published-generation switches. | One count per applicable response; fixture total unknown. |
| 466: short-name index lacks kind/status | **Deferred.** `mod.rs:9538` is still single-column; `15379` uses `+kind` to force that access path. A composite index may improve reads but adds refresh/cold-write costs; needs plan and WAL evidence first. | Unknown; unchanged. |
| 467: OR/instr predicates defeat symbol indexes | **Deferred.** `mod.rs:7554`, `7607` retain Rust trait-suffix matching ORs. Splitting queries must retain representative ordering and overload ambiguity. | Unknown; unchanged. |
| 255: global SQLite filesystem gate around opens/checkpoints | **Deferred across shared DB infrastructure.** `db/file_identity.rs:227-229` still has one reentrant gate; `mod.rs:4859` takes it for atomic-swap work. Removing it changes file-set mutation safety, not just callgraph performance. | Unknown contention/open count; unchanged. |
| 427: published-view opens per operation and quadratic gap dedupe | **Deferred.** `context.rs:7115` opens the pinned-view path; checkout runtime already reuses its reader at `7092`. `response_finalize.rs:720` still uses linear `contains` per gap. Generation/pin ownership and stable gap ordering need targeted tests before changing them. | One open per applicable pinned-view operation; gap comparisons unknown. |
| Live ledger: store-sized incremental refresh/checkpoint | **Not reproducible on current revision.** Delta writes and idle checkpoint throttling already exist; neutralizing equal-row predicates reproduces amplification and trips the new guard. Production sequence/revision remain unknown. | 32 / 256 files: 92 / 90 frames, 29 changed rows each. Mutation: 158 / 302 frames, 277 rows each. |

## Verification and limitations

Toolchain: Cargo 1.99.0 (`5f94df478`), rustc 1.99.0 (`b940084d7`), rustfmt
1.10.0-stable. Package-scoped builds ran sequentially; no Windows cross-check
was run, as requested. The linker emitted the host's oversized `__eh_frame`
warning; it did not prevent compilation.

After the request to isolate storage, test commands set `AFT_STORAGE_DIR` to
fresh directories under `tmp/perf-callgraph`. Earlier counting, mutation and
corpus tests used their own temporary source/store fixtures. No package manifest
or lockfile was changed.

| Command / check | Result |
| --- | --- |
| `cargo test -p agent-file-tools --lib callgraph_store:: -- --test-threads=1` | 171 passed, 0 failed, 6 ignored |
| `cargo test -p agent-file-tools --lib views::materialization:: -- --test-threads=1` | 49 passed, 0 failed, 3 ignored; includes dispatch projection, cold/incremental parity and crash-recovery proofs |
| `cargo test -p agent-file-tools --lib callgraph_store::perf_tests:: -- --nocapture --test-threads=1` | 12 passed, 0 failed, 2 ignored; counts recorded above |
| `cargo test -p agent-file-tools --bin aft -- callgraph` | 6 passed, 0 failed |
| `cargo fmt --all -- --check` | exit 0, silent success; version shown above |
| `cargo test -p agent-file-tools --lib callgraph -- --test-threads=1` | 350 passed, 4 failed, 6 ignored; baseline with isolated storage has the **same four failures**, 346 passed, 5 ignored |
| `cargo test -p agent-file-tools --test rest -- callgraph` | 75 passed, 8 failed, 6 ignored; isolated-storage baseline has the **same eight failures**, 75 passed, 6 ignored |
| Scoped `aft_inspect` | partial rust-analyzer warm-up diagnostics; package compilation/tests are the authoritative compiler checks |

The four broad-lib failures are
`views_owner_rebuilds_stale_callgraph_with_multiple_sessions_and_linked_worktrees`,
`callgraph_and_inspect_dirs_are_root_keyed`,
`views_keyless_callgraph_generation_preserves_unused_exports_but_names_dead_code_gap`,
and `views_tier2_keyless_generation_reports_callgraph_disabled_not_empty`.
The eight acceptance failures are the borrowed-disclosure mismatch fixture and
the seven legacy migration/fallback fixtures named in `gate-rest.log`. They
reproduce on frozen, unchanged base code with the same storage override. The
root-keyed fixture explicitly expects its configured storage path, whereas
`AFT_STORAGE_DIR` supersedes that path; migration fixtures likewise expect their
private configured legacy directories. They were not rewritten to accommodate
the environment. A first baseline command had a failed `mktemp` substitution
and consequently an empty override; it passed 350 tests, but is **not** used as
the isolated-storage baseline evidence. The corrected baseline reruns are the
ones reported in the gate table.

Evidence lives in this worktree's ignored `tmp/perf-callgraph/`: frozen
`input-aft`, `input-fd`; `base-aft`, `head-aft`, `base-fd`, `head-fd`; selective
mutation logs; `gate-{callgraph,store,materialization,work-counts,bin,rest}.log`;
and `baseline-{callgraph,rest}-isolated.log`. The real-repository probe is
reproducible with `AFT_CALLGRAPH_MEASURE_ROOT` and
`AFT_CALLGRAPH_MEASURE_DUMP` set, using:

```sh
cargo test -p agent-file-tools --lib perf_audit2_real_repo_output_equivalence -- --ignored --nocapture --test-threads=1
```

The byte comparison reads the four named artifacts from distinct before/after
directories and asserts equality. There is no reliance on elapsed time for any
work-budget or equivalence result.
