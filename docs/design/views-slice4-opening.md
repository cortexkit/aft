# Slice 4 opening: durable base references

This note freezes the opening contract for slice 4 under the chair ruling
approved on 2026-10-25, not a production enablement or a performance pass.
No product code or tests change with this note.

## Main prerequisites and inherited work

The initial main check is at
`2d4360bd915fd5c8cfcb2dc91f3522c1cc69c25c`. The following prerequisites are
landed ancestors of that commit:

| Prerequisite | Landed evidence |
|---|---|
| Slice 2, eviction handoff | #641, `2adabce3c7fe4c5f29eacb7b7b7b14cf47e6e76b` |
| 3a-1, namespace and reader floors | #665, `926193bec2b1c3255cb8d788600ac495f8f9f321` |
| 3a-2, registry, ledger and admission | #675, `d88966148d864f2c5763f89b399acb7d5db42b6c` |
| 3a-3, retention collector | #719, `28c6648f8c7f92551c3079ca5a1343093536b279` |
| 3a-4, replay and isolation | #742, `d09c9b924ef4e3e767728ef18bea69628cb1d31f` |
| 4a, semantic base-reference prototype | #696, `b054606bbf919451ca4622fd87e83ddede554adb` |
| 3b, rowid payload layout | #777, `bd6f968da5f3c1aafb986f555edb7805121eac20` |

The 3a exit artifact, [views-v3-format.md](views-v3-format.md), records the
registry and plane tables (including `payload` and `payload_digest`), both
`application_id` values, PRAGMAs for each open path, the reservation ledger,
lock ordering, open modes, allocated-byte accounting, budgets and reader floor,
and the full release-walker audit. Its replay result is passing, naming
`blob_store::v3::replay_tests::replayed_copying_closure_is_whole_or_absent_after_any_power_cut`,
the reuse-only and failed-floor-sync retry tests, and the missing-closure-flush
red control. Its deferred implementation register assigns no work to 3a.
This is inherited evidence, not a claim that this documentation slice reran
3a's ignored release-binary scenarios or native durability gates.

3b merged before slice 4 starts, so it holds no `crates/aft/src/blob_store/`
file during slice 4. Slice 4 preserves its layout dispatch and copy-source
no-migration contract rather than introducing another payload layout.

The [4a note](views-slice4a-note.md) has verdict **adjust**, with parity passed.
An adjust verdict permits slice 4; it does not waive 4a's cost failures. If a
4a delivery or the note records failed parity instead, stop the campaign,
notify the operator and re-plan; neither go nor adjust can override failed
parity. The landed note and merge history are the evidence used here.

Slice 4 builds on, and does not reintroduce:

| Existing change | Main merge |
|---|---|
| Immediate superseded-generation cleanup, #844 | `4450354a3bfed9004edf56f2dd1d5fd3aefe7ff0` |
| Single-flight publication, #835 | `b9f261101c9112fee5cbca97cccbbf783c112d55` |
| Incremental manifest, #846 | `05cd281860ec6448d7ae14a98e8be6fdd3c92964` |
| Cheaper cold builds, #841 | `bb67e1db36d57cc8dfbe93beb025e74e091c207e` |
| In-place derived writes, #870 | `d6948dc7a86b71de41685f95900838cacda1fc74` |
| Call-graph extraction shrink, #698 | `5702b091dcf2becdb1effc66d60903e3374df8ac` |

Retention-sweep reference sidecars (#894) are not in the checked main ancestry.
Once they land, rebase onto them: the retention sweep must use their reference
inventory, and superseded-generation cleanup must retire the associated
sidecars with the generation. Do not add a competing inventory or retirement
mechanism. Each implementation slice rebases on current main before merging.

## Four required implementation conditions

These conditions adopt the views design's corrected contracts for shared
bases, base references and durability, as settled by its final pre-build
review. The 3a exit artifact overrides older design spellings and paths.

### C3 — terminal cleanup and generation-aware base edges

`base_refs` is a conservative reverse-edge index, not an owner-lifetime pin.
Every acquisition has its own nonce. On an in-process terminal abort, remove
that acquisition's uncommitted `intent` row under the registry barrier, then
release its marker. Lost CAS, failed verification and strict fallback must not
leave rows alive merely because the daemon is alive. A job whose pointer was
published but whose resident installation was refused keeps its published row;
the durable worker generation still names the base.

Reconciliation keeps or repairs a published row only for the exact marked
`(worker_scope, worker_generation, base_scope, membership_digest)` edge, not
merely a worker/base pair. Coalesce duplicate acquisitions for the same edge;
retire superseded generations' rows once they are no longer current, pinned or
read-marked. Deregistration and base reclamation remove the corresponding rows
under the same barrier as member removal, so rebuilding the same identity
cannot inherit a stale hold. An aborted intent or an old publication must not
hold a base forever or make rows grow with publication count.

Reconcile only rows older than the sweep epoch; keep concurrent refreshed rows
conservatively. Persist progress and bound both the row-producing iterator and
elapsed time using the pre-slice-4 ruling. A capped pass never treats unvisited
rows as absent. A dangling edge records strict rebuild for that exact worker
generation; I/O uncertainty aborts deletion. A live build claim protects even
an empty registered base. Reclamation requires the two consecutive
unreferenced sweeps and barrier recheck of rows, claims, markers and pins.

### C4 — complete journal replay plus epoch, not raw revision

There is one installed-bundle authority in `CheckoutDriver`. Record the expected
resident bundle identity (`seq`, resident worker generation, base scope and
generation) separately from the durable worker generation used for pointer CAS.
Build and open new readers outside the installed mutex. Verify ownership and
the exact acquisition marker immediately before CAS. Only `Published` proceeds;
`Conflict` and `EquivalentWinner` release draft readers, clean up the intent
under the barrier, drop the marker and schedule a fresh, revalidated job. Never
install the winner using the losing job's verification.

After `PointerSynced` and `ReferrerPublished`, take the root index write lock
and replay **all** journal entries after the cut through the locked journal
head. Check the watcher/journal epoch and expected base; then, under
`installed`, check the exact expected bundle identity, swap and increment
`seq`. The freshness fence is complete replay plus epoch, **not raw revision
equality**. Apply the same rule to fold's early revision rejection, so sustained
edits cannot starve installation. A refused install may leave the durable
pointer ahead of the resident bundle, but disk state alone never establishes
checkout readiness.

Queries own reference-counted `PlaneReaders` and exact-generation markers
through their last use, including lazy artifact opens. Releasing the old
binding must not release readers still owned by an old query. Cross-process
readers report the durable `PublishedFrontier`; only the owning driver may
claim freshness for the live checkout.

### C5 — Ready override keys protect their bytes

For v3, `ready_keys()` is the deduplicated union of Ready keys in `entries`
**and** `readiness_overrides`. Family marking, generation marking and post-fold
live-pin trimming all use this union. An override fill may leave its live pin
only after a durable published generation protects the key. Composition parity
and a mutation omitting override keys must exercise this protection.

A checkout-specific readiness exception lives independently of content equality
in `LiveDelta`'s readiness patches and the durable override section; equality
with inherited bytes must not erase it. Workers fill delta paths and their own
readiness exceptions, never inherited base-pending entries. The base claim
owner single-flights those fills and publishes monotone readiness successors.

### C10 — membership-keyed identity, `head_tree=None`, no `git_oid`

Slice 4 constructs bases from strict, stability-checked reads. Identity is a
**tagged verified membership-and-content digest**, together with family,
subtree, effective filter/config/ignore policy, producer/schema/model inputs
and relevant platform inputs. Set `head_tree=None` and leave every `git_oid`
absent/unproven. A caller's tree ID, equality of missing IDs, a worktree's
`.git` pointer and Git's stat cache are not byte proof.

Dirty main may seed this content-addressed base without being labelled a clean
Git tree; every worker is strict-reconciled. A base has fixed membership and no
base reference of its own. Slice 4v may introduce a distinct tree-keyed scope
only after proving the stored bytes as Git blobs and pairing trusted stat/byte
observations with their conversion policy. Never relabel an existing
membership-keyed scope as tree-keyed.

## Adopted 4a changes

Slice 4 adopts **all five** items in the 4a note's “What slice 4 should change”
section:

1. Replace process-only acquisition with independent v3 base scopes, full
   effective identity, depth-one durable references, unique exact-generation
   markers, a referrer index, recovery and reference-aware GC/eviction. An `Arc`
   cannot protect bytes after process death, main deletion or foreign GC.
2. Preserve strict membership/content identity until 4v proves Git blob identity
   and conversion-policy provenance for trusted observations, as C10 requires.
3. Add composed delta entries, tombstones and readiness overrides while retaining
   base-owner single-flight fills and readiness successors. Unlike the prototype's
   fully-ready-only adoption and wholesale fallback, a lexical-ready base may
   have honest pending graph and semantic planes.
4. Enforce publication/install fences and query-owned reader lifetimes. An empty
   delta, losing CAS or equivalent winner never creates a falsely verified ready
   installation.
5. Keep the corrected public witness. Later proven watcher/stat verification
   must reduce repeated inherited-byte checks, and migrating lanes must eliminate
   duplicate work; skipping folds alone left 19,941 instrumented hashes per bind.
   Investigate RSS with private-memory and PSS accounting without waiving the
   measured regression or attributing it to an unmeasured allocation.

The adjust evidence remains visible: bind-to-ready improved 21.55% against a
30% target, hashes plus folds fell 17.87% against 50%, and sampled peak RSS rose
7.62% against a 5% allowance. Semantic parity passed, including the pinned-model
public comparisons, but those cost gates did not. These results do not establish
all-plane parity, cross-platform performance or a verified-bind protocol.

## Rulings and frozen bounds

The **chair family-limit ruling of 2026-10-25** keeps each repository family's
**2 GiB target and 4 GiB hard limit**. After the landed #698 shrink and #870
in-place derived writes, the 01:50Z measurement of the live store found the
largest family referenced 1.0 GiB (about 1.26 GiB on disk across 21
checkouts), with every family within target. These values come from the
maintainers' internal measurement and are reproduced here so the published
design is self-contained; this is not a new measurement.

No slice changes a family-limit or budget constant. The 3a namespace budget is
4 GiB per family, and the device free-space floor is **max(4 GiB, 5% of device
capacity)**, not the older proposed 2 GiB floor. Charge reservations before
writing, retain committed charges until a covering measurement, and refuse a
hard-limit breach with the named G4c storage gap, never a legacy fallback.

## Pre-slice-4 ruling (2026-10-25)

The maintainers approved the pre-slice-4 chair ruling on 2026-10-25, including
the 1 GiB reservation-cap amendment. Its binding values are reproduced below
so the published design is self-contained. They apply to slices 4–7d. **Judgment** identifies a frozen
chair choice, not an achieved measurement or passing gate; the “measured by”
obligations remain with the implementing slices.

| Freeze | Approved chair value |
|---|---|
| Item 24 live-claim wait | **600 s wall time to first lexical-ready publication**, from encountering the claim; immediate ACK and direct-scan service. **5 s heartbeat / 15 s stale inspection** triggers owner-liveness checking, never takeover of a live, SIGSTOPped or uncertain owner. Heartbeats and takeovers never reset the binder clock. |
| Item 24 live-base cap | **64 per family**, including claimed/registered-but-unpublished bases; byte admission remains independently binding. Cap hits use a larger nearest-base delta and are counted. |
| Referrer reconciliation | **256 rows or 25 ms elapsed per family pass, whichever comes first**, cumulative across transactions; the IMMEDIATE-barrier hold also has a **25 ms** budget. |
| Initial build reservation | **`ceil_MiB(64 MiB + max(64 KiB × N, 2 × B, 1.25 × P × max(N/Np, B/Bp)))`**, capped at **1 GiB**, then extended atomically in **256 MiB** steps before measured growth exceeds the reservation. Omit the history term when unavailable or when `Np` or `Bp` is zero. Admission refuses growth past the hard limit, never a base solely because the uncapped estimate exceeds it; existing budget/free-space-floor reasons still apply. |
| G6 drain/admission | **600 s maximum queue/backlog age; 64 distinct pending admission identities per storage root.** Terminal refusal names are `storage_gap:admission_deadline` and `storage_gap:admission_queue_full` (recorded as proposed names in the ruling). |
| G4b takeover fixture | **`aft_lexical_base_owner_death`**, AFT **`05d40ac417db6137a4204521bd3c59853325e968`**, **5,377 files / 87,031,923 source bytes**. Kill after lexical publication and worker binds, before a readiness successor; complete within **1,800 s of the kill**, including takeover. |
| G1s, unchanged | **At least 1,000 binds / at least 20 roots / 14 days**, operator-extendable, on the operator's macOS APFS machine. Zero false-ready mismatches; one mismatch stops the trial and blocks slice 5. A short sample without extension is no-go. |

For the reservation formula, `N` and `B` are the membership file count and
source bytes. `P` is a comparable previous base's measured **peak allocated
build footprint**, with inventory `Np` files and `Bp` source bytes.
`ceil_MiB` rounds upward to a whole MiB. Without history, this AFT fixture
initially reserves **401 MiB**. Charge allocated registry/WAL/SHM, segments
and staging; commit replaces estimates with measured growth, and failure keeps
orphan bytes charged. The 1 GiB cap is a quarter of the 4 GiB hard limit, not
permission to exceed it or to write ahead of an atomic extension.

| Judgment call | Basis and still-required “measured by” evidence |
|---|---|
| 600 s live-claim wait and 5/15 s cadence | The wait is twice Kubernetes's 300 s first-indexed-lexical target, not a measured wait. Fresh-base semantic observations (1,029.435 s baseline / 969.324 s prototype) do not measure first lexical publication. **Measured by:** cold first-publication p95/max across corpora and platforms. Expiry chooses an own strict generation, not takeover; bound workers do not restart this wait for successors. |
| 64 live bases | About three times the reported 21-checkout largest family; checkouts are not bases. The underlying working-set receipt was unavailable to the ruling author and still needs recovery. **Measured by:** distinct live-base counts and their storage/RSS footprint. The cap never overrides the 2/4 GiB family bounds. |
| 256 rows / 25 ms reconciliation | Leaves room within G2a's 100 ms CPU pass budget, but elapsed and CPU time are distinct. **Measured by:** contended p95/max barrier hold and drain throughput. Budget acquisition, SQL and commit as well as iteration; an actual hold overrun fails the gate, not a claimed hard-preemption guarantee. |
| Reservation coefficients and 1 GiB cap | #698's aggregate shrink and single payload example do not estimate all planes; #870 changes derived writes, not payload size. **Measured by:** post-shrink per-file p50/p95/max and whole-build peak allocated bytes across corpora, including no-history runs. The uncapped 200,000-file estimate is about 12.5 GiB, which is why initial reservations are capped and extended from measured growth. |
| 600 s / 64-identity G6 queue | Coalesce identical demands without resetting age; run bounded demand-driven passes. **Measured by:** near-4-GiB, more-than-200,000-file runs with concurrent families, republications and no reclaimable space: maximum age/depth, terminal refusal time and drain completion. Missing the drain horizon fails G6 even with timely refusal. Uncertainty and two-sweep reclaim remain safety requirements. |
| G4b kill rendezvous and deadline | This is a judgment, **not a passing takeover receipt**. **Measured by:** kill-to-complete/max, exactly one successful takeover and disjoint attempt/token names. Retain Kubernetes's separate 7,200 s full-semantic obligation. |

For reconciliation, mark outside the barrier, then iterate indexed rows with
`updated_epoch < S`. Unprocessed bases stay uncertain; resume only under the
same epoch, lease and marked snapshot. Reaching a cap is not evidence that the
remaining rows or references are absent.

The takeover fixture uses the fixed prewarmed MiniLM from 4a, with model
downloads excluded and reported separately. Park the base after
`PointerSynced`, with graph/semantic pending; let workers reach
`ReferrerPublished`, then SIGKILL the builder before `BaseSuccessorPublished`.
With **no new binds**, exactly one surviving worker must detect confirmed death
and claim `attempt + 1` within **600 s of death**, using fresh
`(attempt, owner_token)` staging and derived names. Finish the pending planes
within **1,800 s of death including takeover**, with cold-equivalent answers
and uninterrupted lexical service. The 600 s death-detection bound is measured
from the kill, distinct from a new binder's claim-encounter clock.

## Previous release, storage namespace and handoffs

The newest v0.61 tag at the initial slice-4 main check is **`v0.61.0`**, commit
**`8aceb397b5d9836004b548ea1a5aa8f1d75cd0e8`**. This is the pinned previous
release for slice 4's CLI and daemon coexistence scenarios, not an unversioned
installed binary. Main and the newest-v0.61 tag selection were rechecked after
the ruling was recorded; neither changed during the opening-note hold.

Reuse 3a's complete walker audit and namespace-isolation tests, extending the
actual-binary sweep to this previous release over a root containing a v3 base
and a live referencing worker. The old release must not delete or alter v3
bytes or modes. Same-build foreign processes, both a views-on daemon and a
views-off standalone process over the same storage root, honor base references
through the new protocol. Delete main, kill its daemon, run foreign GC and
restart the worker: the worker must retain correct answers. Builds older than
the pinned previous release are unsupported over a root holding v3 data.

The only v3 family root is `<storage>/blobs/v3/<family>/`; base and worker member
directories are `views/<scope>/` beneath it. Nothing may appear under
`<storage>/views/v3` after base build, bind, republish or reclamation. Use the
registered `views_v3_registry` and `views_v3_store` domains, their ledger/lock
protocol and the existing `evicting` binder contract. A binder must never reuse
an evicting directory.

The [deferred implementation register](views-v3-format.md#deferred-implementation-register)
is written before any owning slice writes its new format: slice 4 owns the
manifest header/frontier, tombstones, readiness overrides, staging names,
plane-row sweep and `DiskBudget` wiring; 4v owns persisted stat evidence and
token rows; 5 owns donor seals; 6a owns the symbol plane and import ledger.
The register separates future format assignments from already-written 3a/3b
formats and does not authorize lazy schema creation.

New implementation remains harness-only for an unrouted entrypoint: the storage
root must contain `views-harness.marker` and the harness selector must choose
it. No new config key or production route is introduced. The differential
views-off/views-on boundary remains equal to pre-slice main for answers, v3
inventory and floor entries until that entrypoint's routing slice lands;
4v's isolated, answer-neutral G1s shadow is the sole specified exception.
New code writes no legacy v1/v2 rows, manifests, pointers or schemas.

This opening changes documentation only. The maintainer landing this slice adds
release-note fragments in the landing commit; this documentation slice does not
write them. Later slice gates, native-platform evidence and operator signoffs
remain obligations, not passes granted by this note.
