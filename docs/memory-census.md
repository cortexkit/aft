# AFT memory census contract

`memory.census` is a passive management operation. It is not an agent tool and
is not a channel-0 control operation. A subc client discovers it in AFT's
`management_surface` provider role, opens a route to
`RouteTarget::ManagementSurface { module_id: "aft" }`, and sends this body on
the bound route channel:

```json
{"op":"memory.census"}
```

The bind requires a first-party principal (`Direct` or a reserved CortexKit
module id), acknowledges in one round trip, and creates no project actor,
configure request, watcher, session registration, or background subscription.
Untrusted binds receive `route_refused`. The immediate bind acknowledgment
matches insula's management server (`crates/quota-module/src/main.rs:555-594`),
and route replies echo the request channel, epoch, and correlation as its
`usage.get` handler does (`main.rs:845-993`). The operation reads the health
worker's published snapshot; it does not enumerate processes or walk allocator
statistics. AFT's consumer-verified wire reply is:

```json
{"op":"memory.census","status":"ok","data":{"roots":{},"process":{}}}
```

`subc-protocol` 0.10 defines the provider at
[`manifest.rs:55-59`](https://docs.rs/crate/subc-protocol/0.10.0/source/src/manifest.rs)
and each operation as `{name, kind}` at `manifest.rs:145-157`;
`memory.census` is a `query`. An undeclared operation or an agent-tool envelope
on this route is refused with `unknown_management_op`.

The `data` payload is:

```json
{
  "roots": {
    "/worktree": {
      "root": "/worktree",
      "root_id": "/worktree",
      "bound_routes": 0,
      "last_request_age_ms": 0,
      "idle_ttl_ms": 1800000,
      "lsp_idle_ttl_ms": 600000,
      "evictable_in_ms": 1800000,
      "planes": {"search": 0, "semantic": 0, "symbols": 0, "callgraph": 0, "inspect": 0},
      "attributed_bytes": 0,
      "evictable_bytes": 0,
      "lsp_children": {"count": 0, "rss_bytes": 0}
    }
  },
  "process": {
    "phys_footprint_bytes": null,
    "rss_bytes": 0,
    "allocator_slack_bytes": 0,
    "allocator_slack_label": "address-space slack: allocator-mapped bytes minus in-use bytes; includes free pages already returned to the OS, so it is not resident memory",
    "sqlite_bytes": 0,
    "total_attributed_bytes": 0,
    "unattributed_bytes": 0,
    "last_relief_at_ms": null,
    "last_relief_allocator_accounting_bytes": 0,
    "last_relief_allocator_accounting_source": "glibc_mallinfo2_size_allocated_drop",
    "last_relief_rss_drop_bytes": null,
    "last_relief_phys_footprint_drop_bytes": null
  }
}
```

`roots` is never capped. `evictable_in_ms` is null while a root has a bound
route, and otherwise is the configured root idle TTL minus its request age
(clamped at zero). `evictable_bytes` is the artifact, symbol, and inspect data
released by the idle reaper, not a second estimate.

`allocator_slack_bytes` is address-space slack: allocator-mapped bytes minus
bytes in use. It is not resident memory. Allocators return free pages to the OS
without unmapping them (glibc `malloc_trim` uses `madvise(MADV_DONTNEED)`
inside the heap; macOS libmalloc marks pages reusable), and neither says which
free pages are still resident, so slack can exceed RSS. The same figure appears
as `retained_slack_bytes` in status and as `allocator_slack_bytes` (tagged
`allocator_slack_kind: "address_space_not_resident"`) in the health rollup.
`unattributed_bytes` is footprint minus attributed bytes, or on platforms
without a footprint (Linux) RSS minus attributed bytes minus allocator slack;
because slack includes pages already returned, that RSS fallback can read low.

The `last_relief_*` fields describe the latest periodic allocator relief pass.
There is no single "freed" number because the two sides disagree:

- `last_relief_allocator_accounting_bytes` is what the allocator's own
  accounting says; `last_relief_allocator_accounting_source` names it. On Linux
  (`glibc_mallinfo2_size_allocated_drop`) it is the drop in
  `mallinfo2().arena + hblkhd`, which misses the pages `malloc_trim` returns
  with madvise and so often reads near zero. On macOS
  (`malloc_zone_pressure_relief_return`) it is the byte count
  `malloc_zone_pressure_relief` returns.
- `last_relief_rss_drop_bytes` is the observed RSS drop across the pass
  (floored at zero; null before the first pass). On Linux this is the figure
  that shows what the trim actually returned.
- `last_relief_phys_footprint_drop_bytes` is the observed footprint drop
  (macOS only). macOS relief lowers the footprint right away but can leave the
  pages in RSS until the kernel reclaims them, so there it is the better
  observable.

The periodic relief pass triggers on at least 1 GiB of slack, at most once
every five minutes. Because slack does not fall when pages are returned by
madvise, a daemon that once crossed the line keeps running a pass every five
minutes; that cadence is bounded and each pass returns what was freed since the
last one, so the trigger was left as is.

`aft profile --memory` renders this contract when supplied a daemon census. In
standalone mode it reports that no shared process exists to attribute rather
than fabricating a census. The `ck health aft --memory` client can delegate to
this operation and render the same payload.
