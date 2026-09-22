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

The budget is a module-side constant, because only AFT knows what one root costs. Its value is **not yet set**. It will be derived from a measurement, not chosen:

- On the live daemon, measure per-root artifact load time for the actual live root set: p50, p95, and the distribution of resident size, since load time tracks artifact size.
- The budget should cover the p95 root across the live set at the parallelism warm-up actually gets, and never exceed a hard ceiling. A module stuck warming refuses every session, which is worse than today's behaviour, so the ceiling protects against that.

Until the measurement exists, this note proposes the shape, not the number.

## Failure behaviour

- **The live-root query is unavailable or errors:** flip ready immediately. This degrades to today's behaviour; it must never degrade to a module that stays not-ready.
- **The query reports bindings whose root is unknown:** warm the roots it does know, and leave the unknown ones to warm lazily. Never read an unknown root as an empty set (see "Bindings with no stored root").
- **Warm-up hangs or runs long:** the budget flips ready regardless, and the unfinished roots continue lazily.
- **A bind arrives while not ready:** readiness is best-effort, because the daemon reads it under a different lock from the relay reservation, so one `on_bind` can arrive before the flip. The existing lazy path already serves a bind to a cold root; no special handling is needed.
- **The module restarts mid-warm-up:** it starts over from HELLO. Nothing persists from a partial warm-up.

## Open questions

1. **Client behaviour on `module_warming`.** A session whose route.open is refused while the module warms must retry, not fail the tool call. The plugin's subc transport already retries transient attach failures with backoff under a 60 s budget. Whether `module_warming` is classified as transient there has to be verified in `packages/aft-bridge/src/subc-transport.ts` before this ships.
2. **Warm order.** If the daemon can report route count or last activity per root, warm the busiest roots first. Optional.

## Daemon half (SUBC)

Facts from SUBC's reading of the daemon (2026-09-22):

- The daemon does **not** keep the root today. `RouteBinding` (`forwarding.rs:62`) holds the client and module channels, epochs, principal and `bound_at`, with no project root. `route.open` canonicalizes the root (`control.rs:2463`), relays it to the module in the bind, and the router then discards it.

Daemon obligations, both SUBC's (not yet built):

1. Store the canonical `project_root` on `RouteBinding` and on the pending reservation.
2. Add a channel-0 query that reads it back per module, which the incoming module can call while still not ready. HelloAck would freeze a snapshot at registration, too early for a swap: the incumbent keeps taking routes while the candidate warms.

Still to specify: the query's name and reply shape, and how it behaves during a swap (whose route set it reports, and when traffic moves).

### Bindings with no stored root

A route opened before the daemon carries obligation 1 has no stored root. The query must report such bindings **explicitly**, not leave them out, and AFT must treat "a binding whose root is unknown" as "warm lazily", never as "no roots". Otherwise the first swap after that daemon cut would get an empty set, warm nothing, and flip ready at once: an absent answer read as an empty one.

So the reply has to distinguish three cases: a root is known and bound, a binding exists but its root is unknown, and the module has no bindings at all. Only the third means there is nothing to warm.
