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
      "planes": {"search": 0, "semantic": 0, "symbols": 0, "callgraph": 0, "callgraph_projection": 0, "inspect": 0, "pending_install": 0, "exact_memo": 0, "compression_filters": 0, "bash": 0, "lsp": 0, "parser_pool": 0},
      "exact_memo": {"status": "ready", "bytes_status": "partial", "estimated_bytes": 0, "entries": 0, "candidates": 0, "file_digests": 0, "descriptors": 0, "lifecycle_keys": 0, "literal_presence_entries": 0, "payload_budget_bytes": 8388608, "not_estimated": ["in_flight_verifiers_and_detached_readers"]},
      "compression_filters": {"status": "ready", "bytes_status": "partial", "estimated_bytes": 0, "filters": 0, "command_keywords": 0, "not_estimated": ["compiled_regex_automata"]},
      "semantic_worker_bytes": 0,
      "semantic_overlay_bytes": 0,
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
    "shared_semantic_base_bytes": 0,
    "semantic_arena_bytes": 0,
    "semantic_resident_chunks": 0,
    "semantic_arenas": [
      {"storage": "/storage", "family": "<family>", "runs": 0, "chunks": 0, "bytes": 0}
    ],
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

`roots` is never capped. Before the first successful health-worker census,
`roots` is null, not an empty map: no roots have been measured. Process I/O can
still be present because its kernel counters do not require the executor. A
contended scheduler retains the last successfully sampled memory census rather
than replacing it with an empty one; the accompanying `health.check` metrics
describe its freshness with `stale`, `stale_reason`, `generated_at_ms`, and
`age_ms` (see [health freshness](ops/aft-health-sentinel.md#census-freshness-and-unknown-values)).

`semantic_worker_bytes` is the private index payload
owned by semantic refresh threads for that root and is **already included** in
`planes.semantic` and the attributed totals. It sums overlapping retiring and
replacement workers until they exit; detaching a worker handle does not erase
its allocation. Shared bases contribute zero to this field and remain counted
once in the process shared-base total. Estimates update on worker creation and
after each refresh (including failed refreshes); transient in-flight buffers,
allocator capacity/size-class overhead and embedding models are not included.
The richer status semantic estimate separates `serving_index_bytes` and
`worker_index_bytes`.

`exact_memo` attributes the root's retained exact-search candidates, duplicate
evidence descriptors, file digests, and key/presence metadata. It estimates
backing capacities, not only live element counts, without reading source files.
The exact-result payload budget is 8 MiB per root context; literal-presence
answers and retired poison keys have separate 1 MiB budgets. Entry-count limits
remain in force. A new generation retires the old generation of the same query,
tests flag and evidence scope. An oversized result is served in full without
being retained: these are cache limits, never ranking-input or result limits.
Idle artifact eviction releases the memo; a busy verifier defers non-blocking
eviction. In-flight verification and pages still held by readers after eviction
are explicitly outside this retained-cache estimate.

`compression_filters` attributes each root's private registry tables, strings
and vectors. Shipped filters share immutable compiled regex engines across all
roots; user/project overrides are still loaded independently on each reset.
The regex library does not expose engine allocation sizes, so compiled engines
remain an explicit estimation gap, not a guessed per-pattern byte count.
The root's `attributed_bytes` includes both cache estimates, plus the bash and
LSP estimates now also exposed in each root row's `planes` JSON object.

With views enabled, a root serves semantic search from its checkout view.
`semantic_overlay_bytes` is that view's own overlay (replacement entries not
yet folded into a generation) and is **already included** in
`planes.semantic`. The vectors the view reads live in its family's arena,
which every view of the family shares. Each family's arena is reported once in
`process.semantic_arenas` (`runs`, `chunks`, `bytes`, largest first); their sum
is `semantic_arena_bytes`, counted once in `total_attributed_bytes`.
`semantic_resident_chunks` is the number of resident chunks, one embedding
vector each, for comparison with allocator block counts. When the idle reaper
evicts a root, it also releases the root's semantic view: the arena drops the
runs no other view of the family still reads, and the root's next semantic
query reports that semantic search is reloading until the view is loaded again.

With the data layer on, each root's checkout in it is counted in the planes
that read it: the text plane's bytes in `planes.search`, the call graph's in
`planes.callgraph`, and meaning search's in `planes.semantic`, each also named
`data_layer_bytes` in that plane's detail. `planes.data_layer` is what those
planes share: the checkout state lists (the publication's, the current one
while edits are recorded, and the ones the call graph's engine and its last
refresh resolved over), each edited path's record, and the publication's open
segments and snapshot. Until the older index stores (the trigram search
index, the semantic index and the call-graph store) are removed, a plane adds
what its older store holds to what the data layer holds for it, since both
hold memory. The row's `data_layer` object itemises
the figures (`search_bytes`, `callgraph_bytes` with `callgraph_engine_bytes`
among them, `semantic_bytes`, `text_overlay_bytes`) and the counts behind them;
the first three are already in their planes, not more to add. What each part
holds:

- text: the publication's per-path text membership and section handles, each
  published path's memoised text-index status, the overlay of edited contents'
  trigrams, and the text members trigram lookups share;
- call graph: the publication's stored slices with every slice decoded so far,
  the graph served with the files resolved since laid over it, the last
  refresh's parses and resolutions, the dead-code snapshot (unless
  `aft_inspect`'s projection holds the same one: `planes.callgraph_projection`
  counts it then), edited files' parses, and the incremental engine's decoded
  parses and resolutions, bound files and the inputs of each pass;
- meaning: the vector generation the plane loaded (its snapshot, the index of
  every vector's text digest, each chunk map's holder, the segments it opened)
  and the members of the view it searched last. The vectors themselves are
  read through the block cache, and a local embedding model is counted once
  for the process with the embedding sessions.

The block cache every checkout reads through is shared, so it is counted once,
in `process.data_layer_block_cache_bytes` (with `data_layer_block_cache`:
`resident_bytes` of block data within `budget_bytes`, block and hit counts);
its figure is the cached blocks plus the cache's lookup tables.

Every data-layer figure is measured, not estimated from counts: the census
walks the live structures and adds up what each part owns on the heap, from
the containers' own lengths and capacities (a string or vector its capacity
times its element size; a hash map the table its capacity allocates; a shared
allocation once however many places hold it). Two limits are stated rather
than guessed around: a B-tree map is counted as full nodes, because the
standard library does not say how full its nodes are, so its figure is a lower
bound; and the allocator's own bookkeeping is not counted. The walk never
waits for a lock: a part another thread holds is left out, counted as
`locked_parts`, and named in `not_estimated`, which makes the figure
`partial`. When the idle reaper releases a root's checkout, its next census
reports `bound_checkouts: 0` and no data-layer bytes.

`evictable_in_ms` is null while a root has a bound
route, and otherwise is the configured root idle TTL minus its request age
(clamped at zero). `evictable_bytes` is the artifact, symbol, inspect and
data-layer checkout data released by the idle reaper, not a second estimate.

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
