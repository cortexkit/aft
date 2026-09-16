# In-situ materialization, 2026-09-16

## Before-fix capture

Captured at 17:17:59Z with the placed `~/.local/share/cortexkit/bin/ck-aft`
(16:25 build), unchanged product, fresh worktree-local storage, and
`AFT_VIEW_PROFILE=1 scripts/views-branch-drill.sh --mode views-on` against
`~/Work/OSS/opencode`. HEAD was `5716f8ba60e79ec60ec485b6e5291c0b0bc1f252`,
A `a085bf62a459c21d90a89bcae006e969df60d944`, B
`b85cf3d67fe3d36a8d04f4247fc781045d5b3dee`. Exclusive subject lock was held
outside the checkout for the entire drill, including restoration.
Raw evidence is worktree-local `.bg-shell/materialization-baseline/output/`.
No product fixes precede this capture. The placed binary's exact source revision
was not independently verified; these are not measurements of a rebuilt task branch.

### Phase breakdown (milliseconds)

The existing publication event already exposes nested phase timings from
`crates/aft/src/views/materialization/profile.rs`. `assembly.rs` brackets clone,
materialization call, and closure. Importantly, **`derived_ms` includes closure**;
adding closure to derived double-counts it in this binary.

| phase | HEAD→B graph | HEAD→B fill | B→HEAD graph | B→HEAD fill |
|---|---:|---:|---:|---:|
| manifest assembly | 3085 | 107 | 2963 | 149 |
| blob phase | 653 | 361 | 1478 | 883 |
| derived total (includes closure) | 9993 | 3692 | 22245 | 6954 |
| clone | 13 | 27 | 13 | 87 |
| materialization call | 7511 | 67 | 17825 | 162 |
| load bindings / dependent selection | 828 | 3 | 2687 | 9 |
| delete rows | 1223 | 0 | 3443 | 0 |
| owned blob decode / insert | 616 | 0 | 1922 | 0 |
| selected join (inclusive) | 2168 | 0 | 5071 | 0 |
| ↳ decode/bind index entries | 883 | 0 | 3138 | 0 |
| ↳ index / surface replay | 57 | 0 | 122 | 0 |
| ↳ decode resolved callers | 565 | 0 | 607 | 0 |
| ↳ resolve / record | 579 | 0 | 1016 | 0 |
| ↳ dependency union | 50 | 0 | 112 | 0 |
| write bindings | 261 | 0 | 610 | 0 |
| emit refs / edges | 1429 | 0 | 2299 | 0 |
| transaction commit | 151 | 2 | 283 | 2 |
| closure | 2467 | 3596 | 4403 | 6704 |

Selection, deletion, emission, and transaction commit are in `materialization.rs`;
join timers use the same `materialization/profile.rs` collector from the callgraph
join. The current counters do not isolate manifest diff, memo hit rate, index
rebuild, or fsync. Transaction commit is not a standalone fsync measurement.
Consequently this capture does **not** prove cold cache versus regression against
the offline 3.78 s figure. The graph materialization calls range from 7.511 to
17.825 s, and host load from 20.44 to 28.17, versus the supplied 10–16 baseline.
Index/surface replay alone is only 57/122 ms; row deletion and emission together
are 2652/5742 ms. Attributing the entire gap to resolver memo warmth is unsupported.

The fill does enter the materializer, but `manifest_callgraph_equivalent` already
skips graph rows and only advances metadata. Assembly still clones the derived
generation, opens a keeper, schedules its checkpoint, and validates full closure.
The measured fill bottleneck here is closure (3596/6704 ms), not graph emission.

### Four rows against supplied baseline

| switch | supplied views correct s | capture correct s | supplied views CPU s | capture CPU s | embeds | reported puts |
|---|---:|---:|---:|---:|---:|---:|
| HEAD→A | 15.6 | 37.191 | — | 66.18 | 3 | 0 |
| A→HEAD | — | 14.491 | — | 79.05 | 0 | 0 |
| HEAD→B | 22.2 | 24.758 | 49.2 | 62.03 | 1 | 0 |
| B→HEAD | 24.1 | 43.022 | 56.2 | 56.38 | 0 | 0 |

All four probes report correct, no defects, and return legs report zero embeddings
and puts. However, `puts` is not trustworthy on forward rows: HEAD→A's event
reports 269 puts and HEAD→B's reports 13, while the drill summarizes zero. The
A→HEAD window also contains three publications, including a pending-path count
that rises from 185 to 272. Row timing is therefore retained as observed, not
claimed to be a clean apples-to-apples acceptance result. No target is claimed met.

## Semantic-fill change and verification

The parent confirmed that the placed baseline executable was card 103 from
`651311547c3b`; the retained executable is `/tmp/card-wt/target/release/aft`.
The baseline stands, with the inclusive accounting correction above. This task's
candidate is built from task base `f5b953a33` plus the semantic-fill change, so it
is not a binary-identical baseline rebuild.

Commit `e5c3da010` makes a semantic-only publication reference the previous durable
derived owner through `derived-<generation>.ref`. It does not clone, open a derived
writer, change derived metadata, schedule a derived checkpoint, resync the derived
database, or checkpoint aliases. Closure validates only newly referenced blob
keys and includes only semantic blob durability. The existing full publication
path is unchanged. The next graph edit clones the actual owner and uses that
owner's manifest for its fingerprint precondition. Sweeping retains owners of
references and may reclaim an obsolete owner one sweep later. Ownership-reference
creation and sweeping serialize through the pointer transaction so a publisher
cannot release its base pin after an out-of-date ownership snapshot.

`crates/aft/src/callgraph_store/mod.rs::ReadonlyCallGraphStore::open_manifest_view`
needed the allowed materializer seam change: readers previously constructed a
`derived-<generation>.sqlite` filename directly and now resolve the shared owner.
No configure or runtime-drain code changed.

### Red-first and non-vacuity evidence

The strengthened existing test
`view_assembly_wiring_test::semantic_plane_follows_an_immediately_published_callgraph_plane`
failed before the fix with `semantic fill must reuse the durable callgraph database,
not clone or checkpoint it` (different generation paths). It now also checks
sweeping and a subsequent graph edit.

Mutation runs staged the live implementation before each change, confirmed an
empty working diff, applied a `NON-VACUITY BREAK`, captured the nonempty diff,
and restored from the index followed by `touch` and an empty working diff.

| control | named failure | result |
|---|---|---|
| Force old fill materialization | `views::assembly::semantic_fill_tests::semantic_fill_has_no_derived_writes_or_checkpoint_and_reader_uses_owner` | First attempt passed: the unit fixture had not tracked lib.rs. Fixed fixture to commit lib.rs and assert a pending initial publication and a completed fill. |
| Force old fill materialization, corrected fixture | same test | FAILED: `InvalidManifest("sqlite error: derived metadata rewritten")`; 0 passed, 1 failed |
| Schedule a shared-graph checkpoint | same test | FAILED: `semantic fill scheduled a callgraph checkpoint`; 0 passed, 1 failed |
| Remove publisher ownership serialization | `views::generation::ownership_tests::ownership_reference_waits_for_sweep_pointer_lock` | FAILED: `ownership reference escaped the sweep lock`; 0 passed, 1 failed |

No mutation remains. These controls establish actual derived-write avoidance and
checkpoint omission rather than just a matching path or a no-op publication.

### Gates

- `cargo test -p agent-file-tools --test view_assembly_wiring_test`: unavailable
  in this base; the file is a module of the `integration` target.
- Equivalent integration module: 6 passed. Publication/CAS, migration,
  schema, pins/GC, and staging selected integration modules: 44 passed.
- `cargo test -p agent-file-tools --test watcher_integration branch_switch`:
  2 passed (134.18 s). This real-watcher matrix is intentionally registered in
  the serial watcher binary, not the parallel integration binary. An accidental
  duplicate registration in integration/main.rs was reverted before committing.
- Views plus executor publication units: 56 passed, 3 opt-in benchmarks ignored.
- Callgraph join and executor publication units: 13 passed, including disk/manifest
  resolver row parity and publication/query isolation.
- `cargo rustc -p agent-file-tools --lib -- -D warnings` and the corresponding
  `--bin aft` command: passed on the final product code.
- Release build: passed. AFT inspection timed out in tier-2 rescan, so no clean
  AFT diagnostic result is claimed.
- `cargo clippy -p agent-file-tools --lib --bin aft -- -D warnings`: failed with
  63 existing warnings promoted to errors, including search-index
  `manual_is_multiple_of`, `chunks_exact_to_as_chunks`, and documentation list
  indentation. No unrelated lint cleanup was attempted. Compiler deny-warnings
  passed separately.
- The separately described 69-fixture gate was not identified/run as such;
  the parity tests above are not represented as a substitute fixture count.

## After-fix drill: blocked before transitions

Attempted the same exclusive-lock, fresh-storage, views-on drill with the locally
built `target/release/aft`. Storage:
`.bg-shell/materialization-after/storage`; output:
`.bg-shell/materialization-after/output/branch-drill-views-on.stderr.log`.
The drill exited 1 during the cold readiness phase before producing any switch
rows:

```text
cold work readiness failed while requesting the tier-2 dead-code pass:
inspect could not complete: inspect_request_timeout: tier2_rescan could not
complete within the 120000ms request budget (5000ms terminal reserve)
Completed phases: 98.
```

The existing drill restored the subject and the wrapper released its external
lock. There are **no after-fix four-row measurements**, and neither the ≤5 s graph
materialization target nor the relative CPU/time acceptance is claimed achieved.
The semantic-only unit is verified locally; its in-situ savings remain unmeasured.
Graph-changing publication still has the baseline selection/deletion/emission
costs shown above. No additional graph optimization or resolver cache rewrite is
included. The offline 3.78 s discrepancy remains unresolved, rather than being
attributed to load without evidence.

Next action: rerun with a fresh storage directory when the cold tier-2 readiness
pass can finish on this host, retain the clone/materialization/closure split, and
obtain the exact 69-fixture parity command from the owner. A change to the drill's
cold-readiness contract or runtime scheduling is outside this slice's product
changes. Review this delivery as a coherent **partial semantic-fill improvement**,
not a completed performance-target result.
