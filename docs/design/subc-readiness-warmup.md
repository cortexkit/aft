# Module readiness and warm-up for blue/green swaps

Status: draft for operator review. AFT half written by AFT; the daemon half is SUBC's to fill in (marked below). Nothing here is built.

## Problem

When the aft module is replaced (a card placement today, a blue/green swap later), the new process starts with no artifacts resident. Every session rebinds, AFT acknowledges each bind immediately, and the heavy loading (search index, symbol cache, callgraph store, semantic index) runs afterwards in deferred maintenance. Many roots then load at once and contend for the executor's maintenance workers and the two cold-build slots. For a while after the swap, the first tool call on a root pays for that root's loading.

Blue/green swap is what the operator asked for. Its point is that the incoming module should take traffic only once it is warm, so that nobody sees the swap.

## Non-goal

This does not address the fleet-wide "did not answer route.bind within 12s" failures. Each of those was a specific executor defect, now fixed: long bash waits pinning maintenance workers, a $TMPDIR sweep that collected 290k entries before applying its cap, and a fleet health census rebuilt synchronously inside RouteBind. A readiness gate would not have prevented any of them.

## Mechanism

The subc daemon (0.20.6) supports a module registering with `ready: false` on HELLO and flipping to `ready: true` via `catalog.update`. While a module is not ready, `route.open` to it answers `module_warming` with `detail.reason: "declared_not_ready"`.

AFT would:

1. Send `ready: false` on HELLO.
2. Ask the daemon which roots have live or pending routes (daemon half, below).
3. Warm those roots under a fixed budget.
4. Flip to `ready: true` when warm-up finishes or the budget runs out, whichever comes first.
5. Let any root not warmed by then warm lazily on first use, exactly as today.

## What "warm" means for AFT

Warm-up loads artifacts that already exist on disk for a root. It never starts a cold build: a root with no persisted artifacts cannot be warmed cheaply, and a cold build during warm-up would hold a cold-build slot for minutes (an accidental cold build of the Linux kernel tree held one for 4 h 50 min on 2026-09-22). Such roots are skipped and take the normal lazy path.

Warm-up must also respect the existing admission rules, so it cannot starve itself: it runs as maintenance-class work and takes cold-build limiter slots like any other loader.

## The budget

The budget is a module-side constant, because only AFT knows what one root costs. It differs by how the module was started, because the two cases differ in who is waiting.

**What was measured** (2026-09-22, the aft drain-restart at 21:12Z, from the new process's log): the existing lazy path loaded 25 symbol caches between 21:12:39 and 21:13:25 (46 s) and 17 semantic indexes between 21:12:40 and 21:13:19 (39 s), with loads admitted through the two cold-build slots while sessions were already rebinding and working. So warming the live set with today's admission took about **45 s end to end**.

Caveats, so the number is not over-read: the log has one-second resolution and no per-load duration, so this is observed wall time for the whole burst, not an intrinsic per-root cost. It includes queueing behind the two-slot limiter and contention from live traffic. It is an upper-bound shape, good enough to size a budget, not a benchmark.

**Plain restart.** Callers see `module_warming` while the module is not ready. Both subc SDKs retry that code only until the route-open retry deadline (30 s by default), after which the open fails. AFT's plugin transport is built on `@cortexkit/subc-client` (`isRetryableRouteOpenCode` includes `module_warming`), and `aft-bridge`'s error contract also classifies it as transient, so a warm-up window delays tool calls rather than failing them, but only inside that deadline. The measured full warm-up (~45 s) does not fit. Proposed budget: **10 s**, well under the deadline with room for the SDK's retry backoff. Roots not warm by then continue lazily, as today.

**Swap.** The incumbent stays routable until cutover, so nobody sees `module_warming` and the budget does not delay callers. It can cover the full live set. Proposed budget: **90 s**, twice the measured burst, as a ceiling against a warm-up that hangs.

Both are named constants in the module, not daemon config.

## Failure behaviour

- **The live-root query is unavailable or errors:** flip ready immediately. This degrades to today's behaviour; it must never degrade to a module that stays not-ready.
- **The query reports bindings whose root is unknown:** warm the roots it does know, and leave the unknown ones to warm lazily. Never read an unknown root as an empty set (see "Bindings with no stored root").
- **Warm-up hangs or runs long:** the budget flips ready regardless, and the unfinished roots continue lazily.
- **A bind arrives while not ready:** readiness is best-effort, because the daemon reads it under a different lock from the relay reservation, so one `on_bind` can arrive before the flip. The existing lazy path already serves a bind to a cold root; no special handling is needed.
- **The module restarts mid-warm-up:** it starts over from HELLO. Nothing persists from a partial warm-up.

## Open questions

1. **Which budget applies.** The module must know whether it was started for a swap or a plain restart to pick between the two budgets above. Is that visible to it (HelloAck, the live-roots reply, or the launch environment)? If not, it must assume a plain restart and use the 10 s budget, which is safe in both cases.
2. **Warm order.** If the daemon can report route count or last activity per root, warm the busiest roots first. Optional.

## Daemon half (SUBC)

Facts from SUBC's reading of the daemon (2026-09-22):

- The daemon does **not** keep the root today. `RouteBinding` (`forwarding.rs:62`) holds the client and module channels, epochs, principal and `bound_at`, with no project root. `route.open` canonicalizes the root (`control.rs:2463`), relays it to the module in the bind, and the router then discards it.

Daemon obligations, both SUBC's (not yet built):

1. Store the canonical `project_root` on `RouteBinding` and on the pending reservation.
2. Add a channel-0 query that reads it back per module, which the incoming module can call while still not ready. HelloAck would freeze a snapshot at registration, too early for a swap: the incumbent keeps taking routes while the candidate warms.

Still to specify: nothing on the query itself. SUBC's half is written in the subconscious repository at `docs/designs/module-readiness-and-swap.md` (master `d5cb856a`), section "The warm set" and slice E:

- Query `supervisor.live_roots { module_id }` returns `LiveRoots { module_id, roots: [{project_root, bound, pending}], unknown_root_bindings, total_bindings }`. `total_bindings` makes "no routes at all" a positive statement rather than an inference from two zeros. The root is `Option<ProjectRootId>` on the binding, so a pre-change binding stays unknown rather than absent.
- The query describes whichever endpoint is routable when answered: before cutover, the incumbent. The reply is a snapshot; routes the incumbent takes after the query warm lazily after cutover. Re-querying before the flip is allowed; polling is not needed.
- Slice E (store the root, add the query) is daemon-only and independent of the swap machinery, so it can land first.

### Bindings with no stored root

A route opened before the daemon carries obligation 1 has no stored root. The query must report such bindings **explicitly**, not leave them out, and AFT must treat "a binding whose root is unknown" as "warm lazily", never as "no roots". Otherwise the first swap after that daemon cut would get an empty set, warm nothing, and flip ready at once: an absent answer read as an empty one.

So the reply has to distinguish three cases: a root is known and bound, a binding exists but its root is unknown, and the module has no bindings at all. Only the third means there is nothing to warm.
