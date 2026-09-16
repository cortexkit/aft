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
