# Order inventory and traversal guarantees

The baseline is `998c896b05753dd3a83158e30f97779b89ef28e2`.
`order_sql.json` identifies each graph/views SELECT, including unordered reads,
by the stable id in `statements.json`. `source_inventory.json:rust_order` lists
Rust sorts, BTreeMap/BTreeSet and dedup sites, including test-only consumers.
These machine-readable lists are conservative supersets: a count-only query or
a set used solely for membership is still listed rather than silently excluded.

## Replacement-key rules (apply to every listed key, in sequence)

| Baseline key | Legacy replacement | Views replacement |
| --- | --- | --- |
| 64-hex ref/edge/dispatch-hint id | first eight digest bytes, big-endian, sign bit flipped | only for digest-form ids; non-digest forms use reconstructed text below |
| node id | `((class << 63) | (digest_prefix >> 1)) XOR (1 << 63)`, class 0 for `pos:symbol:`, 1 for `top:` | same base-node integer for `view:symbol:<base>` |
| view ref id | not applicable | complete reconstructed `view:<path>:<key>` text under BINARY collation |
| ordinary/dispatch view edge id | not applicable | complete reconstructed `edge:view:...` / `dispatch:...` text under BINARY collation |
| interned path/name/signature/module/full_ref/import name | join string text, not its integer | same |
| fixed-vocabulary kind/status/import-kind/provenance/language/backend/form | vocabulary text or equivalent CASE key, never append-only numeric code | same |
| rowid | rowid table, original insertion order; id must not alias rowid | same, including preserve_rowids updates |
| implicit index order, DISTINCT, GROUP BY | retain old decoded keys and selected representative; inspect baseline plan before changing index | same |

No ordering on non-digest ids can be replaced by ordering on component columns.
For example `view:a.ts.more.ts:1` sorts before `view:a.ts:1`, and decimal `:10`
sorts before `:2`. `dispatch:` sorts before `edge:view:`. These are bytewise
comparisons, not locale or numeric sorting. A site with two dispatch targets
has two edge identities even when both edges share its ref id.

An identity-salted database may reorder only inside groups tied on every key
before the first salted-id key in the listed sequence. A cut can select different
rows only from its straddling tie group. No interned-string salt changes order.
Unlimited non-traversal replies retain the same multiset.

## Reader and selection families

| Consumer | Baseline keys / selection | Baseline source |
| --- | --- | --- |
| direct callers | source node, ref byte start, line, ref id | `callgraph_store/mod.rs:11400-11429` |
| batched callers | target file, target symbol, then direct-caller keys | `mod.rs:11449-11517` |
| outgoing calls and target fallback | source node, byte start, line, ref id; first matching node | `mod.rs:11524-11681` |
| single-file and global node lookup | scoped-name/path/name/kind text, positions and id tie-breaks as each SQL lists | `mod.rs:11154-11264`, `11816-11847` |
| eager/lazy resolver index | scoped name/id; ref byte start/line/ref id | `mod.rs:6568-6626`, `18946-19032`, `19173-19227` |
| per-caller ref resolve / BlobRef join | CallerRefKey/kind; original ordinal groups and first encounter; BTreeSet DerivedRow order | `callgraph_store/join.rs:374-411`, `650-783`, `2574-2667` |
| view materialization | manifest path order, parse symbol order, ordered join rows, pending ref/edge vector order | `views/materialization.rs:659-738`, `912-940`, `1141-1204`, `1994-2106` |
| dispatch edge reconciliation | complete edge id text, first encountered target, language BTreeMap | `views/materialization/dispatch.rs:1122-1205`, `1265-1314` |
| callers/impact presentation | dedup then source/file/site sort; BTreeMap file groups; test filtering and hub cap | `commands/callgraph_store_adapter.rs:609-770`, `959-1138` |
| trace path presentation | BTreeSet path-shape dedup, entry symbol then path length | `callgraph_store_adapter.rs:526-593`, `1151-1339` |
| inspect dead code / unused exports / cycles | all graph SELECT orders and Rust-order entries in the projection and inspect modules, including name/kind/id ties | `callgraph_store/dead_code_projection.rs`; `commands/inspect*` |
| zoom enrichment | AST source order, first same-name reference retained | `commands/zoom.rs:37-53`, `1688-1805` |
| search enrichment | depth-one callers group order, then compact basename suffixes | `commands/semantic_search/mod.rs:9610-9669` |

## Traversal consumers (classification is about visited-state lifetime)

| Entry | Classification | Evidence and salted-order consequence |
| --- | --- | --- |
| adapter/store `callers`, adapter/store `impact` | **whole-traversal** | best-depth map shared across recursive calls; first visit retains sites, shorter revisit may re-expand (`callgraph_store_adapter.rs:2822-2863`; `mod.rs:11266-11315`). Impact enriches callers without a separate path enumeration. No completed-paths guarantee. |
| adapter `call_tree` | **per-path visited**, whole-traversal adjacency memo and node/work budgets | visited entry removed on unwind (`callgraph_store_adapter.rs:2865-3052`); memo caches adjacency, not rendered subtrees. Shared-node occurrences can both expand. Limits still make retained occurrences/order budget-sensitive. |
| adapter/store `trace_to` | **per-path** | queue item owns its path; reject only a caller already in that path (`callgraph_store_adapter.rs:1151-1339`). Below all limits, completed paths and reached-node set are invariant. Expansion budget is 10,000; path retention/depth can change selected paths and exhausted-budget counts. |
| adapter/store `trace_to_symbol` | **whole-traversal** | BFS request-global `(file,symbol)` visited set and first shortest discovery (`callgraph_store_adapter.rs:1356-1472`; `mod.rs:10086-10191`). Salt can select a different equal-length route even below budgets. Reached-node set remains invariant without limits; completed-paths guarantee does not apply. |
| adapter `trace_data` | **whole-traversal** | shared `(file,symbol,variable,approximation)` visited state (`callgraph_store_adapter.rs:1517-1555`, `1606-1681`); AST child/argument order at `1683-1792`, `1948-2089`; forward facts sorted byte-start/end/line at `2121-2155`. No completed-paths guarantee. |
| inspect cycles | **whole-traversal** | graph-wide cycle/component detection, not independent completed caller paths; preserve graph input id/text order and cycle presentation keys. |
| inspect dead-code reachability | **whole-traversal** | shared reachable-node set seeded from roots; preserve root/candidate ordering and budget selection. |

A description of call_tree sharing a rendered visited set does not match the
recorded baseline: at this baseline the adapter removes visits on
unwind. The inventory records the code actually used to generate the goldens,
without weakening that existing per-path behavior. The no-limit reached-node
invariant applies to every traversal; completed-path invariance applies only to
per-path enumeration, not BFS's selected shortest path or data-flow dedup.
