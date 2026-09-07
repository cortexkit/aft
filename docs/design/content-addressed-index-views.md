# Content-addressed index artifacts and per-view assembly (v3 design, 2026-09-04)

Status: v3 after two Athena consults (`ct_…4d2449c5c708`, `ct_…55da31faedf0`). Round 1 ruled R1–R14;
round 2 found four of those rulings not self-sufficient and is answered by R15–R25 below, which
supersede the earlier text where they conflict. The spec campaign fires on this version.

## Problem, with evidence

Every index plane is keyed per checkout: the artifact key (`sha256(sorted root commits)`) selects the
store, and the store holds the files *as they are on disk now*. Three consequences we measured:

1. **Branch switches re-do work and could trigger full rebuilds.** A checkout touching more than
   `WATCHER_BATCH_INLINE_CAP = 256` paths (`crates/aft/src/runtime_drain.rs:1496`) routed to
   `refresh_project_corpus` (`:2581-2591` -> `:1703`), which dropped the resident callgraph store and
   forced a cold rebuild (`:1749`). This repo: 305,297 refs, ~36 refs/s in the resolution stage on
   2026-09-04 = hours at 100% CPU. (The 256-path reaction is fixed independently; the re-do-on-return
   remains: switching back re-embeds and re-extracts identical content.)
2. **Worktrees are frozen readers.** A linked worktree is borrow-only on the main checkout's artifacts
   (`artifact_owner.rs:93-110`, `readonly_artifacts.rs:1-7`); its own edits are invisible to semantic
   search and its callgraph is the parent's. Masons (97.7% of `aft_inspect` calls) live in worktrees.
3. **First load is neither resumable nor shareable for the expensive planes.** The semantic index
   persists only on a completed build (`commands/configure.rs:4107-4171`); a daemon kill mid-embed
   loses everything (#280). Callgraph extraction is redone per checkout.

## Principle

Split each expensive plane into a **per-file part** keyed by content and a **per-view part** keyed by
the checkout:

- `blob` = immutable per-file artifact in a shared append-only store, keyed as ruled in R1/R2.
- `view` = `(project_scope_key, manifest: path -> blob key)` plus the derived per-view tables (callgraph
  resolution = cross-file edges; dead-code projection). Trigram search stays per-view (R11).

## Rulings from the consult

**R1 — Semantic blobs are keyed by `(blake3(bytes), rel_path, chunker_version, model_fingerprint)`.**
The panel showed the embed text bakes `file:<relative>` into the vector input
(`semantic_index.rs:1895-1915, 4579-4609`), so identical bytes at different paths yield different
vectors. Stripping the path changes retrieval quality and forbids importing today's `semantic.bin`
vectors; keeping it forecloses cross-path sharing. Ruling: keep the path in the key. Sharing then holds
for the same path across branches, worktrees, clones and switch-backs — the measured cases — and a
rename costs one re-embed. Invariant 1 is restated as "a blob is a pure function of its key tuple",
with `rel_path` an explicit key component for the semantic plane only. `model_fingerprint` must cover
model id, dimensions, normalization/output format (panel c4). Existing `semantic.bin` vectors import
losslessly on the same root because `IndexedFileMetadata` records the content hash
(`semantic_index.rs:4884-4901`).

**R2 — Callgraph blobs hold unresolved extraction only.** Today `build_file_extract`
(`callgraph_store/mod.rs:8612-8693`) resolves imports, reexports and module refs against
`project_root` and reads other files (tsconfig/package.json via the memo, `callgraph.rs:168-194,
1744`) at extraction time; nodes, refs and hints carry file paths. Ruling: the blob stores the parse
result only — symbols, raw refs with unresolved `module_path` strings, declared modules, exports —
with **blob-local ordinals** for node/ref identity. Path binding, ID materialization, import/module
resolution and every `project_root`-taking helper move into the view join, which already dispatches
through `ResolverIndex` (`mod.rs:2266-2288, 9581-9674`). This is a refactor of the extract helpers,
larger than v1 implied; the spec enumerates every helper (panel ev-8) and its new home. Callgraph key:
`(blake3(bytes), language, extractor_version)`; no path component.

**R3 — Views are keyed by `project_scope_key`, never by the artifact key.** Two worktrees of one repo
share an artifact key but must not share a view (`path_identity.rs:32-50`); keying views by artifact
key would rebuild the frozen-reader problem inside the design. The artifact key is unchanged and now
serves one purpose: the **blob namespace** (R10).

**R4 — Hash domains.** Git blob SHA-1 (over header+bytes) and BLAKE3 over bytes are different
functions; "both are content hashes" was false. Canonical blob content key = `blake3(bytes)`. An alias
table `(git_oid -> blake3)` is filled whenever a clean tracked file is hashed, so subsequent checkouts
resolve most paths from `git ls-tree -r HEAD` with zero reads; a miss falls back to reading and
hashing (still no re-embed if the blake3 is present). Dirty and untracked files hash from the working
tree on the watcher path that already reads them.

**R5 — Determinism (invariant 2).** Equal manifests produce byte-identical derived tables only if (a)
resolution consumes refs in a canonical order — `(caller blob key, ref ordinal)` — not staging rowid
order (`mod.rs:2160-2167`), and (b) every resolution input (tsconfig, package.json, Cargo.toml,
workspace layouts, ignore files) is read from the **manifest's blobs**, never the live filesystem.
The module-resolution memo is per view (it is already snapshot-scoped, `callgraph.rs:96-99`); sharing
it by repo identity would import another checkout's config state. Open question 2 is closed.

**R6 — Same-root manifest publication is serialized.** Generation-swapped pointers give atomic
visibility, not lost-update prevention (`mod.rs:7836-7858`). One writer per view at a time:
compare-and-swap on the manifest generation (publish fails if the base generation moved; the loser
re-derives from the new base). Cross-root blob puts stay lease-free.

**R7 — Puts are hash-verified and atomic; reads verify.** `INSERT OR IGNORE` alone lets the first
wrong writer poison the machine-global store permanently. Each put is one SQLite transaction carrying
the payload and a completeness marker, and the key is recomputed from the payload before insert;
readers verify `key == hash(payload)` on read and treat a mismatch as absent (and log it). Idempotent
and atomic are different properties; the spec claims both and tests both (crash mid-put leaves no
partial row).

**R8 — Durability before visibility.** Order: commit every referenced blob (WAL, `synchronous=NORMAL`
minimum, stated in the spec), fsync, write and fsync the immutable manifest generation, rename the
pointer, then expose. A manifest may only be published when every blob it names is durable; while
blobs are pending the *previous complete* generation stays current and the desired generation reports
per-path pending/failed (`Ready{refreshing}` shape, `context.rs:887-896`). Admission (limiter,
breaker) may delay or refuse work; it never publishes an incomplete manifest.

**R9 — GC is budgeted mark-and-sweep, not refcount.** Roots of the mark: retained manifest generations
(current + previous per view), publication pins written *before* a view starts assembling (the
inspect-sweep and `gc_old_generations` read-marker precedents, `cache.rs:1784-1791`,
`mod.rs:7930-7951`), and a minimum blob age above worst-case assembly time. A byte budget (#210) may
evict only unreferenced blobs; evicting a referenced blob turns invariant 2 into a re-derivation
obligation and is forbidden. View directories of deleted checkouts follow the existing two-sweep
directory-absence reclaim.

**R10 — Blob namespace = artifact key (repo family).** The panel flagged a machine-global store as a
cross-repo existence oracle readable by untrusted binds (vectors of other repos' files). Ruling: the
blob store is partitioned per artifact key. This keeps every measured win (branches, worktrees, clones,
forks — same root commits) and drops cross-repo sharing of vendored files, which was never a goal. GC
enumeration is then bounded to one family's views.

**R11 — Trigram stays per-view**, published under the same view generation so a query never combines
trigram state from one manifest with semantic/callgraph state from another (panel).

**R12 — Trust.** Only daemon-side first-party code performs shared puts (borrow-only linked worktrees
included — this is a deliberate weakening of `readonly_artifacts.rs:4-7`, stated as such); `mcp:*`,
`Unverified` and `fed:*` binds never put and read only their own family's namespace, with an explicit
bind-to-capability rule in `subc/mod.rs` and tests. Quotas per family bound disk exhaustion.

**R13 — Refresh runs on plane workers, never in watcher slices.** The watcher slices
(`runtime_drain.rs:2032-2362`) invalidate and collect paths; hashing, embedding and extraction go to
the plane workers exactly as `SemanticRefreshRequest::Files` does today, preserving park/replay on
unbind and fixing the existing drop path (`:2325-2337`, which loses staleness records). Breaker keys
move from `(root, domain, corpus_fingerprint)` to `(family, plane)` for blob work so one root's
suspension cannot starve blobs another view needs, and identical work is admitted once.

**R14 — Migration.** Semantic: import-on-same-root (R1 makes it lossless). Callgraph: rebuild once via
the existing staged cold build. Migration is one-way with the old artifacts retained until the first
view publishes; rollback = delete the view directory.

## Storage (revised)

- Blob stores: `<storage>/blobs/<artifact_key>/<plane>.sqlite`, WAL, `synchronous=NORMAL`,
  `busy_timeout`, rows `(key tuple as PRIMARY KEY, payload BLOB, complete INTEGER, created_at)`.
- Alias table: `<storage>/blobs/<artifact_key>/oid-alias.sqlite` `(git_oid PRIMARY KEY, blake3)`.
- Views: `<storage>/views/<project_scope_key>/manifest-<gen>.json` (+ pointer), derived tables in
  `<storage>/views/<project_scope_key>/derived-<gen>.sqlite`, pins under `pins/`.

## Invariants (restated)

1. A blob is a pure function of its key tuple (R1/R2); nothing outside the tuple influences the payload.
2. A view is exactly `manifest + blobs`; equal manifests yield byte-identical derived tables (R5).
3. Puts are idempotent, atomic and hash-verified (R7); same-root publication is CAS-serialized (R6);
   GC never removes a pinned or referenced blob (R9).
4. Version and model changes never mix: they are key components; old blobs age out, never reinterpret.
5. Only first-party daemon code writes the shared store; untrusted binds read their own family and
   write only their own manifest (R12).

## Non-goals (v1)

Trigram restructuring; cross-repo blob sharing; cross-machine export (the per-family store makes it a
later feature, not a design change); artifact-key changes; tool-surface changes.

## What the spec must specify (from the panel's "would have to guess" lists)

Canonical path and case-folding rules; symlink and submodule handling; deletion and rename semantics;
dirty-file race detection (hash-then-stat); which config/ignore files enter the manifest; blob-local
ordinal construction; SQLite schema, synchronous mode, corruption recovery, `CREATE TABLE` races under
concurrent openers; manifest CAS protocol; query generation pinning; pending/failed-path semantics and
their rendering in each tool; derived-table invalidation; breaker retry behaviour under the new key;
GC roots, retention, quotas, read markers; authorization per bind class; migration rollback;
observability (`index_event` kinds for put/pin/publish/sweep); conformance tests for all five
invariants.

## Relationship to RFC #286

randomvariable's RFC proposes incremental segment indexing, coverage manifests, and CoW worktree
generations. A coverage manifest is this design's view manifest; segments are blob groups. The designs
converge on the split and differ on the unit and on trigram participation. The #286 reply is written
in this frame after the OSS matrix numbers.

## Round-2 rulings (supersede R1/R4/R6/R7/R8/R9/R13 where they conflict)

**R15 — Source key and payload digest are separate (supersedes R7's read rule).** A blob's key is
input-addressed and cannot be recomputed from a derived vector or a lossy parse. Each row therefore
stores, beside the key, `payload_digest = blake3(canonical payload bytes)` and `payload_schema`. Writers
compute both; readers verify `payload_digest` on read and treat a mismatch as absent (logged). The put
remains one transaction with a completeness marker. Source-key attestation (the key was derived from
the bytes it claims) is the writer's obligation under R12; nothing at read time can prove it.

**R16 — Embed template version is a key component.** `build_embed_text_with_lines`
(`semantic_index.rs:4579-4646`) is versioned code outside the chunker and the model fingerprint; the
semantic key becomes `(blake3, rel_path, chunker_version, embed_template_version, model_fingerprint)`.

**R17 — Alias entries are proven, never assumed (supersedes R4's alias rule).** Git symlinks, EOL
normalization, clean/smudge filters and LFS make oid→bytes one-to-many. An alias row
`(git_oid → blake3)` is written only when the daemon has the working-tree bytes in hand AND
`sha1("blob <len>\0" + bytes) == git_oid` for a regular (mode 100644/100755) entry; symlinks, submodules
and filtered paths never alias. The zero-read checkout uses aliases only for rows that passed that
check; everything else reads and hashes.

**R18 — CAS is a SQLite transaction, not compare-then-rename (supersedes R6).** The per-view manifest
pointer is a row in the view's SQLite; publication is `UPDATE pointer SET generation = ?new WHERE
generation = ?base` inside one transaction with `busy_timeout`; zero rows updated = conflict, the loser
re-derives from the new base. No long-lived writer lease; the lock is held for the transaction only,
so a daemon restart mid-publication leaves either the old or the new generation, never both.

**R19 — Pins are durable and owned (supersedes R9's age floor as sufficient).** Before the first put
of an assembly, the view writes a pin `(family, view, generation, owner = pid + process start time)`
and fsyncs it; the sweep treats pinned keys as referenced; a pin whose owner is dead (the `fs_lock`
liveness rule) or older than `PIN_TTL` is reclaimable. The blob age floor stays as a second guard, not
the primary one.

**R20 — Durability composition (extends R8).** Before pointer visibility: blob rows committed and the
plane store's WAL fsynced; derived tables and trigram state for the generation committed and fsynced;
alias rows committed; manifest file written, fsynced, and its parent directory fsynced (today's
semantic write syncs the temp file but not its parent; the pointer path already does both,
`mod.rs:7836-7858`). `synchronous=NORMAL` is the floor; the spec states the exact PRAGMA set.

**R21 — Breaker scope and work dedup are different keys (extends R13).** The breaker is per
`(family, plane)`; work admission dedups on the full blob key and fans completion out to every waiting
view; a blob whose put fails deterministically is quarantined by key so it cannot suspend the plane
for sibling views.

**R22 — The callgraph resolution family is rewritten over manifest data, not moved.** The panel's
enumeration (`import_dependencies`, `collect_reexport_refs`, `collect_rust_pub_use_reexport_refs`,
`build_import_refs`, `build_rust_module_refs`, node/ref ID materialization in `mod.rs`; the
`resolve_module_path*`, `resolve_workspace_module_path`, `resolve_rust_module_path`,
`resolve_file_like_path` family and the Cargo workspace walk in `callgraph.rs`) is the work list. All of
them derive answers from the live filesystem today; at the join they read manifest membership and
manifest blobs (tsconfig, package.json, Cargo.toml as blobs). Symlink topology is recorded in the
manifest (entry kind + target) so canonicalization becomes a manifest lookup; submodules are manifest
entries of kind `gitlink` and are not descended into in v1.

**R23 — Path identity.** `rel_path` in keys and manifests is the exact byte string relative to the
view root, no case folding, `/` separators, NFC not applied; a `path_identity_version` component on
the manifest allows changing this later without silent reinterpretation. Payloads never contain
absolute paths; anything absolute is bound at read from the view root.

**R24 — Invariant 2 is logical.** Equal manifests yield equal derived *row sets* (canonical order,
canonical serialization when compared), not byte-identical SQLite files.

**R25 — Pending and failed refreshes.** Queries serve the previous complete generation and annotate
the paths whose blobs are pending or failed; they never omit paths silently and never mix
generations. The manifest closure includes global ignore state and directory membership so that
"which paths exist" is itself part of the view.

**Open-question verdicts (round 2).** OQ1: keep `rel_path` in the key and in the embed text; the
LeBench A/B (path-in-text vs path-free vs path as a separate rerank feature) decides only retrieval
quality and is scheduled after the first views ship. OQ2: ordinals are deterministic within an
extractor version only; a bump rebuilds derived tables (resumable). OQ3: R18.

**R10 clarified.** Ordinary forks and mirrors preserve root commits and stay in the family; only
history-rewritten mirrors (filter-repo, squashed re-creations) pay a full index. Stated so it is not
rediscovered as a regression.

## Open questions for round 2 (historical; answered above)

1. R1 keeps `rel_path` in the semantic key. Is there a retrieval-quality argument for stripping the
   path from the embed text (and taking the one-time re-embed) that outweighs lossless migration?
   Decide with an A/B on the LeBench retrieval set, not by argument.
2. Blob-local ordinals (R2): stable across extractor versions or not? If not, every extractor bump
   invalidates derived tables for all views of the family — acceptable?
3. R6 CAS vs a per-view writer lock: CAS needs re-derivation on conflict (cost = one resolution pass);
   a lock serializes but reintroduces a lease. Which failure mode is preferable under daemon restarts?
