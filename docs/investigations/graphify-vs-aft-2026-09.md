# Graphify vs AFT (code-level investigation, 2026-09)

Scope: Graphify-Labs/graphify at `33362d9` (`v0.9.53`) and AFT at this repository's current source. I treated Graphify's prose as intent and its implementation/tests as authority. “Graphify” below means the Python distribution named `graphifyy`, whose package metadata installs a `graphify` CLI (`graphify/pyproject.toml:1-10`). It is a knowledge-graph builder and query layer, not an embedding/vector index: its code query ranks lexical terms (including IDF and trigram candidate generation), selects graph nodes, then traverses edges (`graphify/graphify/serve.py:300-322`, `graphify/graphify/serve.py:369-442`, `graphify/graphify/serve.py:1197-1256`).

## 1. Pipeline map

### 1.1 Code-repository path, end to end

```text
root / files / URL
  -> recursive discovery + layered ignores + file classification
  -> deterministic per-file structural extraction
       generic tree-sitter engine OR a language-specific extractor
       nodes + edges + unresolved raw_calls + per-language resolution facts
  -> corpus-wide identity cleanup and resolution
       imports/re-exports -> stub rewrites -> direct calls -> typed member calls
  -> NetworkX graph construction / validation / deduplication
  -> undirected community projection -> Leiden (or Louvain fallback)
  -> graph.json + GRAPH_REPORT.md + graph.html
  -> CLI query/explain/path, MCP tools, and generated /graphify skill
```

#### Discovery and ignore rules

`detect()` recursively scans a file or directory, classifies supported files, reports code/document/paper/image/video counts, and returns the actual path lists (`graphify/graphify/detect.py:1665-1716`, `graphify/graphify/detect.py:1853-1894`). Built-in noise pruning recognizes dependency/build/cache directories and evidence-backed virtual-environment, coverage, and snapshot directories; dot-directories are not blanket-dropped (`graphify/graphify/detect.py:925-963`, `graphify/graphify/detect.py:1798-1835`). Regular-file and sensitive-path gates run before classification (`graphify/graphify/detect.py:1868-1886`).

The ignore predicate composes `.graphifyignore`, optionally `.gitignore`, nested ignore files, and CLI extra excludes (`graphify/graphify/detect.py:1553-1637`, `graphify/graphify/detect.py:1715-1745`). Its matcher implements last-match-wins negation, anchored/basename/path glob forms, and Git's parent-exclusion rule; ignored directories are pruned before descent (`graphify/graphify/detect.py:1386-1524`, `graphify/graphify/detect.py:1828-1835`). Symlinks are skipped by default; when enabled, out-of-root targets and loops are rejected (`graphify/graphify/detect.py:1640-1662`, `graphify/graphify/detect.py:1774-1785`, `graphify/graphify/detect.py:1836-1844`).

Classification is extension-oriented. PDFs are `paper`; common raster formats are `image`; prose/office formats are `document`; media formats are `video`; a wide source/config extension set is `code`; unsupported or extensionless non-shebang files are recorded as unclassified (`graphify/graphify/detect.py:44-49`, `graphify/graphify/detect.py:503-537`, `graphify/graphify/detect.py:1886-1893`). This classification is broader than “tree-sitter code”: Markdown and selected config formats also have deterministic structural extractors.

#### Per-language extraction

The generic engine takes a `LanguageConfig` naming a tree-sitter module plus class, function, import, call, boundary, name and body node kinds (`graphify/graphify/extractors/models.py:13-57`). It always emits a file node; configured class-like declarations become nodes connected by `contains`; function/method declarations become nodes connected by `contains` or `method` (`graphify/graphify/extractors/engine.py:3004-3044`, `graphify/graphify/extractors/engine.py:3070-3167`, `graphify/graphify/extractors/engine.py:4194-4238`). Type positions add `references` edges with contexts such as `field`, `parameter_type`, `return_type`, `generic_arg`, and `attribute` (`graphify/graphify/extractors/engine.py:4244-4353`). Calls are attributed to the containing callable; same-file targets become `calls`, while unresolved targets are retained as `raw_calls` for corpus resolution (`graphify/graphify/extractors/engine.py:5519-5632`). Python/JS callback values can also become the distinct `indirect_call` relation, and PHP adds framework-specific relations such as `uses_config`, `bound_to`, `listened_by`, and static/constant references (`graphify/graphify/extractors/engine.py:5634-5756`, `graphify/graphify/extractors/engine.py:5758-5804`, `graphify/graphify/extractors/engine.py:5903-5977`).

The grammar/config core is:

| Inputs | Grammar/configured declaration kinds | Configured call kinds |
|---|---|---|
| Python | `tree_sitter_python`; `class_definition`, `function_definition` | `call` |
| JS | `tree_sitter_javascript`; classes, function/generator declarations, methods | `call_expression`, `new_expression` |
| TS/TSX | `tree_sitter_typescript` with distinct TypeScript/TSX entrypoints; classes, abstract classes, interfaces, enums, type aliases, functions/methods/signatures | `call_expression`, `new_expression` |
| Java | `tree_sitter_java`; classes, interfaces, records, enums, annotation types, methods/constructors | invocations and object creation |
| Groovy/Gradle | `tree_sitter_groovy`; class/interface and method/constructor declarations | method invocations |
| C / C++ | `tree_sitter_c` / `tree_sitter_cpp`; functions plus C++ classes/structs | call expressions |
| Ruby | `tree_sitter_ruby`; class/module and method/singleton-method declarations | `call` |
| C# | `tree_sitter_c_sharp`; class/interface/enum/struct/record and methods | invocation and object creation |
| Kotlin | `tree_sitter_kotlin`; class/object and functions | call expressions |
| Scala | `tree_sitter_scala`; class/object/trait and functions | call expressions |
| PHP | `tree_sitter_php`; class/interface/enum/trait and functions/methods | function/member/scoped/class-constant calls and object creation |
| Swift | `tree_sitter_swift`; class/protocol and function/init/deinit/subscript declarations | call expressions |
| Lua | `tree_sitter_lua`; functions | function calls |

Those declarations are literal config values, not inferred from README language badges (`graphify/graphify/extract.py:785-919`, `graphify/graphify/extract.py:934-1044`, `graphify/graphify/extract.py:1070-1129`). The dispatch table also routes Go, Rust, Zig, PowerShell, Elixir, Objective-C, Julia, Fortran, Vue, Svelte, Astro, Dart, OCaml, Common Lisp, Verilog, SQL, Markdown, Pascal, Bash, JSON/config, Terraform/HCL, DreamMaker assets, .NET project/XAML/Razor, Robot, and Apex to specialized extractors (`graphify/graphify/extract.py:5368-5471`). Therefore “tree-sitter AST for code” is directionally right but incomplete: some specialized extractors use other parsers or structured/regex logic, and optional extras can be required for SQL, Terraform, DM, OCaml, Common Lisp, or Robot (`graphify/graphify/extract.py:5474-5500`).

Every structural result is stamped `_origin: "ast"` and reports zero LLM tokens (`graphify/graphify/extract.py:7256-7266`, `graphify/graphify/extract.py:7293-7301`). The separate semantic extractor explicitly states that documents/papers/images—not source files—reach the model; code-named nodes from those sources are evidence-checked and flagged rather than silently trusted (`graphify/graphify/llm.py:637-649`). PDFs are converted to text; raster images are sent as pixels only to a vision-capable backend and otherwise still receive reference nodes (`graphify/graphify/llm.py:526-537`, `graphify/graphify/llm.py:823-876`, `graphify/graphify/llm.py:897-930`). Video is first transcribed into a text file by the skill and then handled as semantic text, not parsed directly by the Python extractor (`graphify/graphify/skill.md:114-151`).

#### Cross-file resolution and provenance

Resolution is a sequence of deterministic code passes:

1. JS/TS and Python import/export/use facts are collected, alias and star-export chains are followed, and evidence-backed `imports`, `imports_from`, `re_exports`, inheritance and call edges are emitted as `EXTRACTED` (`graphify/graphify/extractors/resolution.py:822-907`, `graphify/graphify/extractors/resolution.py:937-1067`, `graphify/graphify/extractors/resolution.py:1069-1108`).
2. Node IDs are disambiguated, namespace-sensitive Java/PHP/C#/Go references are resolved before generic stub rewiring, and Python/Java/C# import relations are added (`graphify/graphify/extract.py:6613-6706`).
3. Generic raw calls are matched by exact case (or language-appropriate folding), guarded against cross-language-family matches, and disambiguated first by symbol/module import evidence, then only by conservative test/path tie-breakers (`graphify/graphify/extract.py:6752-6827`, `graphify/graphify/extract.py:6881-7018`). A direct call with import evidence is `EXTRACTED`/1.0; a unique name-based direct call without it is `INFERRED`/0.85; JS/TS calls without import evidence are dropped rather than guessed. Callback-value edges remain `INFERRED`/0.85 even with an import because the syntax referenced rather than invoked the symbol (`graphify/graphify/extract.py:7019-7083`).
4. Language-specific receiver-aware member-call resolvers run through a registry after IDs are final; incremental runs supply unchanged nodes/edges as read-only resolution context (`graphify/graphify/resolver_registry.py:28-85`, `graphify/graphify/extract.py:7085-7107`).
5. A Python-specific pass turns a named import plus an actual reference inside a local class/function into `uses` at 0.95 (`graphify/graphify/extractors/resolution.py:1914-1931`, `graphify/graphify/extractors/resolution.py:1972-2108`).

No LLM participates in these code-resolution passes. The `EXTRACTED`/`INFERRED` values describe evidence strength, not whether a language parser versus model executed; Graphify's semantic prompt uses the same vocabulary and adds `AMBIGUOUS` (`graphify/graphify/llm.py:478-507`).

#### Community detection

The graph is converted to a stable undirected simple graph for partitioning. Graphify first calls `graspologic_native.leiden` with fixed seed 42, one iteration/trial and weighted edges; it falls back to `graspologic.partition.leiden`, then NetworkX Louvain (`graphify/graphify/cluster.py:22-93`, `graphify/graphify/cluster.py:96-166`). Isolates become singleton communities. Optional high-degree hubs are removed and majority-vote reattached; communities above max(10, 25% of all nodes) are partitioned again; communities of at least 50 nodes below 0.05 density receive another split (`graphify/graphify/cluster.py:223-325`). IDs are deterministic size-descending indexes on a fresh build, while update logic can greedily remap them to prior IDs by overlap (`graphify/graphify/cluster.py:318-325`, `graphify/graphify/cluster.py:361-409`). Without an LLM labeler, each community is named after its highest-degree member (`graphify/graphify/cluster.py:175-199`).

#### Storage and incremental behavior

The operating graph is a NetworkX `Graph` by default (or `DiGraph` when requested), built wholly in memory from node/edge dictionaries (`graphify/graphify/build.py:798-824`). The durable primary artifact is node-link JSON, not SQLite: `graph.json` contains sorted node dictionaries, sorted links, graph metadata/hyperedges, per-node community IDs/names, confidence scores, and optionally `built_at_commit` (`graphify/graphify/export.py:266-342`, `graphify/graphify/export.py:343-410`). Sidecars under `graphify-out/` include the Markdown report, HTML, manifest/root/build config, per-file AST JSON cache, semantic chunk cache, community labels/signatures, and optional learning/query logs.

The second-run story is hybrid rather than simply “full rebuild” or “delta”:

- AST cache keys are content-based and version/schema-scoped, so unchanged files need not parse again after ordinary edits; semantic cache entries are separately prompt-fingerprinted (`graphify/graphify/cache.py:21-38`, `graphify/graphify/cache.py:71-120`).
- The interactive `graphify update` calls `_rebuild_code` without a changed-file list, so it discovers the full code corpus and logically rebuilds that tier, relying on per-file cache hits to avoid repeated extraction (`graphify/graphify/cli.py:2385-2429`, `graphify/graphify/watch.py:1305-1325`). Watch/hook callers may pass `changed_paths`; then only changed files are extracted, unchanged graph entries are preserved, and deleted paths are pruned (`graphify/graphify/watch.py:1321-1335`). Concurrent hooks queue path sets under a per-repo rebuild lock (`graphify/graphify/watch.py:1341-1391`).
- Merge semantics are per source and per tier: fresh AST replaces old AST for that source without deleting its semantic layer (and vice versa); unchanged items carry forward (`graphify/graphify/build.py:1652-1680`, `graphify/graphify/build.py:1711-1740`). Incremental resolvers receive unchanged-corpus context so changed-to-unchanged calls survive (`graphify/graphify/extract.py:6763-6783`, `graphify/graphify/extract.py:7085-7107`).
- Unless `--no-cluster` is selected, graph assembly, community detection, report generation and exports still run over the resulting whole in-memory graph. It is extraction-incremental, not a persistent graph database applying edge-level transactions.

#### Artifact shape observed on a tiny code-only fixture

I installed the checked-out Graphify with `uv` into `/tmp/graphify-mason-129-venv`, ran `graphify extract . --code-only --force` on two tiny Python files (`main.py` imports/calls `helper()`), then ran `graphify cluster-only .` to create the report/view. I did not configure an LLM key. The resulting three public files were:

```text
graphify-out/
  GRAPH_REPORT.md  1,031 bytes
  graph.html      16,893 bytes
  graph.json       2,652 bytes
```

The observed JSON shape was:

```json
{
  "directed": false,
  "multigraph": false,
  "graph": {},
  "nodes": [
    {"id":"main","label":"main.py","file_type":"code","community":0,"community_name":"helper","_origin":"ast","source_file":"main.py","source_location":"L1"},
    {"id":"main_run","label":"run()","file_type":"code","community":0,"community_name":"helper","_origin":"ast","source_file":"main.py","source_location":"L3"}
  ],
  "links": [
    {"source":"main_run","target":"helper_helper","relation":"calls","confidence":"EXTRACTED","confidence_score":1.0,"context":"call","source_file":"main.py","source_location":"L4","weight":1.0,"_origin":"ast"}
  ],
  "hyperedges": []
}
```

(The excerpt omits the `helper.py` nodes and remaining links; `built_at_commit` was absent because the fixture was not a Git repository.) `GRAPH_REPORT.md` contained corpus/summary, extraction percentages, commit freshness instructions, community hubs, god nodes, import cycles and community membership—the sections are assembled directly from graph statistics (`graphify/graphify/report.py:128-179`, `graphify/graphify/report.py:197-276`). `graph.html` embedded sanitized vis.js node/link arrays; inferred edges would be thinner/dashed, and the sidebar exposes node search, inspection, community filters and counts (`graphify/graphify/exporters/html.py:501-580`, `graphify/graphify/exporters/html.py:590-637`).

### 1.2 Query surface

The CLI query grammar is natural-language text plus optional `--dfs`, repeated `--context`, `--budget`, and `--graph`; CLI traversal is fixed at depth 2 (`graphify/graphify/cli.py:1202-1244`, `graphify/graphify/cli.py:1299-1318`). MCP exposes `query_graph` (BFS/DFS, depth 1–6, token budget and relation-context filters), `get_node`, `get_neighbors`, `get_community`, `god_nodes`, `graph_stats`, `shortest_path`, and PR-impact tools (`graphify/graphify/serve.py:1614-1697`, `graphify/graphify/serve.py:1698-1743`).

For a question, Graphify removes multilingual filler words, scores label/path/ID matches with exact/prefix/substring tiers and IDF, picks up to three dominant seeds plus one winner per still-relevant term, then BFS/DFS traverses an optionally context-filtered graph (`graphify/graphify/serve.py:218-291`, `graphify/graphify/serve.py:472-639`, `graphify/graphify/serve.py:666-755`, `graphify/graphify/serve.py:1197-1256`). BFS/DFS stops expanding non-seed hubs at max(50, degree p99) but includes edges induced among visited nodes (`graphify/graphify/serve.py:884-962`, `graphify/graphify/serve.py:965-989`). Text output is line-oriented:

```text
Graph: graphify-out/graph.json (N nodes) | Traversal: BFS depth=2 | Start: [...] | K nodes found
NODE Label [src=path loc=Lx community=name]
EDGE Caller --calls [EXTRACTED context=call]--> Callee at=path:Lx
```

The confidence tier is therefore agent-visible per returned edge, not confined to `graph.json` (`graphify/graphify/serve.py:1047-1089`).

### 1.3 Benchmarks: method, numbers, critique

`BENCHMARKS.md` reports a six-question “code navigation” suite and a separate memory benchmark. The code suite used `Qwen3.5-27B-FP8`, a local Qwen3 embedding server, 512k context, three trials, one warmup, cache clearing, median token counts, and exact-match scoring against JSON ground truth; success means all expected symbols appear in the final answer (`graphify/BENCHMARKS.md:16-42`). Reported code results are:

| Configuration | Avg score | Code tokens/query | Total tokens/query |
|---|---:|---:|---:|
| Graphify only | 0.6111 | 140,628 | 153,601 |
| Hybrid retrieval | 0.8333 | 160,622 | 173,617 |
| Baseline tools | 0.8889 | 146,494 | 158,849 |

These values and the report's conclusion that Graphify-only trailed baseline by 27.8 percentage points are in `graphify/BENCHMARKS.md:47-79`. The peak-RSS benchmark reports 0.4/0.6 MB for 100/500 nodes, 2.8 MB for 2,000, 14.2 MB for 10,000, and 72.4 MB for 50,000, from `tracemalloc` around synthetic graph build and 100 iterations of top-10 degree sorting (`graphify/BENCHMARKS.md:88-117`). A separate ERPNext table reports 1,919 files and 664k words, 61/379/15 code/doc/paper files, 27,843 nodes, 21,129 edges, 564 communities, 0.896 average cohesion, 7,700 input and 8,061 output tokens, and $0.0006 estimated cost (`graphify/BENCHMARKS.md:124-157`).

**Methodology critique.** The code-navigation experiment is useful as an end-to-end agent retrieval test: it fixes the model/context, repeats runs, uses symbol-level ground truth, and reports both quality and token consumption. It does not isolate graph construction time, index/query latency, graph freshness, resolver precision/recall, or the contribution of each Graphify mechanism; only six questions are reported, and the agent can use tools iteratively, so 140k “code tokens/query” is not a direct measurement of the 2,000-token graph query budget. The memory test is even narrower: synthetic node-link construction plus repeated degree sorting omits discovery, parsing, cross-file resolution, Leiden, JSON duplication and a resident query server. `BENCHMARKS.md` contains no wall-clock build or query runtime numbers. The in-package `benchmark.py` is a token-size estimator, not a quality/runtime benchmark: when corpus word count is absent it assumes 50 words per node, seeds by label substring, traverses depth 3, and estimates four characters per token (`graphify/graphify/benchmark.py:33-82`, `graphify/graphify/benchmark.py:85-133`).

## 2. Side-by-side with AFT

| Dimension | Graphify | AFT |
|---|---|---|
| Primary purpose | Materializes a heterogeneous knowledge graph, clusters it, exports human/agent views, and supports concept/subgraph queries. Code extraction and code resolution are deterministic; semantic documents/media use an LLM (`graphify/graphify/extract.py:7293-7301`, `graphify/graphify/llm.py:637-649`). | Provides live repository search, structural reading, diagnostics/health, and code-navigation tools. The callgraph is one specialized persisted substrate rather than a universal docs/concepts graph (`crates/aft/src/inspect/job.rs:15-31`; `crates/aft/src/commands/callgraph_store_adapter.rs:438-532`). |
| Languages | A broad heterogeneous dispatch: generic tree-sitter configs plus specialized parsers for additional languages/config formats; optional extractor dependencies exist (`graphify/graphify/extract.py:5368-5500`). | One parser abstraction maps 30 `LangId`s—including code, Markdown/HTML/JSON/YAML—to concrete tree-sitter grammars (`crates/aft/src/parser.rs:693-769`). This is broader as a single consistent outline/parser surface; Graphify's extension dispatch is broader if specialized non-tree-sitter/config formats are counted. |
| Symbols / outline | Nodes are persistence identities in a graph: file, type/container, callable and selected field/config/doc concepts, with `source_file`/line (`graphify/graphify/extractors/engine.py:3004-3044`, `graphify/graphify/extractors/engine.py:4194-4238`). | `SymbolKind` explicitly models function, class, method, struct, interface, enum, type alias, variable, heading and file summary (`crates/aft/src/symbols.rs:3-20`). Parsing caches unchanged files by path/mtime (`crates/aft/src/parser.rs:1557-1563`); outline emits compact nested trees with signatures and a 30 KiB cap (`crates/aft/src/commands/outline.rs:38-189`). |
| Callgraph facts | Per-file nodes/edges/raw calls are dictionaries; relations cover contains/method/import/call/inheritance/reference plus language-specific semantics (`graphify/graphify/extractors/engine.py:3070-3167`, `graphify/graphify/extractors/engine.py:5519-5632`). NetworkX simple-graph construction can represent only one surviving edge payload per node pair (`graphify/graphify/build.py:798-803`). | SQLite tables preserve files, nodes, refs, resolved edges and unresolved refs; refs retain line/byte ranges, status, `resolution_kind`, full callee and caller/target IDs (`crates/aft/src/callgraph_store/mod.rs:1826-1960`). Call sites remain addressable rather than collapsing an entire pair to one NetworkX simple edge. |
| Resolution | Import/export evidence, unique labels, language-family guards, path/test tie-breaks, and language-specific receiver resolvers; confidence 1.0/0.95/0.85 depending on evidence (`graphify/graphify/extract.py:6881-7083`). | Resolution is an explicit outcome: direct resolver success, global `type_match`, unique `name_match`, or unresolved. Type match is preferred; name match is intentionally approximate; unresolved calls stay queryable (`crates/aft/src/callgraph_store/mod.rs:13587-13776`). `StoreCallSite` carries the exact provenance string (`crates/aft/src/callgraph_store/mod.rs:2010-2037`). |
| Persistence | Canonical JSON plus caches/sidecars; full graph rehydrates into NetworkX for build/query (`graphify/graphify/export.py:323-410`, `graphify/graphify/build.py:798-824`). | Persisted, WAL-backed SQLite with schema/version metadata and indexes over node/ref lookup paths (`crates/aft/src/callgraph_store/mod.rs:2080-2180`, `crates/aft/src/callgraph_store/mod.rs:2452-2561`). Readers can open a published generation read-only (`crates/aft/src/callgraph_store/mod.rs:2760-2816`). |
| Incrementality | Per-file content/version cache; CLI update scans the full corpus but skips cached extraction; watch/hook mode can replace/prune changed sources only; clustering/export still run over the whole assembled graph (`graphify/graphify/watch.py:1305-1335`, `graphify/graphify/build.py:1652-1680`). | Watcher refresh updates exactly changed files and returns `IncrementalStats`; transactions update file state, nodes, refs and re-resolution (`crates/aft/src/callgraph_store/mod.rs:3913-3945`). Search and semantic indexes also have file-level update/refresh paths (`crates/aft/src/search_index.rs:1185-1225`, `crates/aft/src/semantic_index.rs:3226-3436`). |
| Cold build control | AST extraction uses a process pool sized by explicit/env/CPU limits (Windows capped at 61) and falls back to serial on pool failure; no repo-wide AST time budget is enforced in extraction (`graphify/graphify/extract.py:5730-5815`). | `ColdBuildLimiter` enforces per-stage/total elapsed budgets, file caps, resume offsets and a circuit breaker; progress is staged and persisted so later requests can resume (`crates/aft/src/callgraph_store/mod.rs:1041-1119`, `crates/aft/src/callgraph_store/mod.rs:2952-3077`, `crates/aft/src/callgraph_store/mod.rs:3088-3215`). |
| Worktrees | Graph artifacts live under the analyzed project/output location; the implementation has rebuild locks but no Git-common-dir borrowing layer (`graphify/graphify/watch.py:1341-1391`). | Linked worktrees are detected from `git-dir` vs `git-common-dir`, routed to read-only borrowing before ownership claims, and open the shared published generation without rebuilding (`crates/aft/src/commands/configure.rs:881-936`, `crates/aft/src/artifact_owner.rs:93-110`, `crates/aft/src/callgraph_store/mod.rs:2760-2785`). |
| Search | Query is lexical seed scoring plus graph traversal; its trigram index is an in-memory candidate accelerator, not an embedding index (`graphify/graphify/serve.py:369-442`, `graphify/graphify/serve.py:472-639`). | Search has a persisted trigram/postings index with base+delta snapshots and per-file updates (`crates/aft/src/search_index.rs:295-362`, `crates/aft/src/search_index.rs:1063-1225`). Semantic search stores symbol-aware chunks and embedding vectors, checks model/dimension fingerprints, and performs cosine top-k search (`crates/aft/src/semantic_index.rs:1895-1915`, `crates/aft/src/semantic_index.rs:2208-2248`, `crates/aft/src/semantic_index.rs:3485-3564`). |
| Inspection / dead code | Reports god nodes, surprising connections, import cycles, ambiguous edges, communities and graph gaps (`graphify/graphify/report.py:197-324`). It does not implement AFT's fresh diagnostics/dead-code inspection contract. | Inspect separates diagnostics/metrics/TODOs from Tier-2 dead code, unused exports, duplicates, cycles and complexity (`crates/aft/src/inspect/job.rs:15-31`, `crates/aft/src/inspect/job.rs:71-87`). Store-backed dead-code liveness deliberately requires real call edges and is narrower than parser coverage (`crates/aft/src/inspect/job.rs:371-382`). |
| Navigation | Natural-language neighborhood, node, neighbors, community, hubs, stats, shortest path and PR impact; returned edges include relation/confidence/location (`graphify/graphify/serve.py:1614-1743`, `graphify/graphify/serve.py:1817-1879`). | `callers`, `impact`, `call_tree`, `trace_to`, `trace_to_symbol`, and `trace_data` are code-specific operations backed by the callgraph (`crates/aft/src/commands/callers.rs:11-75`, `crates/aft/src/commands/impact.rs:11-93`, `crates/aft/src/commands/trace_to_symbol.rs:11-145`). The plugin tells agents when to use each and how to disambiguate (`packages/opencode-plugin/src/tools/navigation.ts:26-85`). |
| Context control | Query defaults to about 2,000 tokens, keeps seeds first, cuts at line boundaries with explicit truncation/count/narrowing advice, and may deliberately exceed budget when all nodes fit so it does not silently omit edges (`graphify/graphify/serve.py:992-1147`). | High-fanout callers/impact summarize once total fanout exceeds 20 and retain 15 representative entries; trace expansion is bounded to 10,000 prefixes (`crates/aft/src/commands/callgraph_store_adapter.rs:23-30`, `crates/aft/src/commands/callgraph_store_adapter.rs:479-505`). Outline is byte-capped and reports incomplete discovery (`crates/aft/src/commands/outline.rs:38-106`). |
| Human visualization | `graph.html` provides a searchable whole-graph/community view; above 5,000 nodes the exporter can aggregate to a community meta-graph or refuse, depending on call mode (`graphify/graphify/exporters/html.py:14-30`, `graphify/graphify/exporters/html.py:396-479`). | No equivalent whole-callgraph HTML/community view exists in the cited AFT engine/tool surface. |
| Non-code knowledge | Semantic nodes/edges/hyperedges can come from documents, PDFs and images; video is represented through transcription (`graphify/graphify/llm.py:478-507`, `graphify/graphify/llm.py:823-930`, `graphify/graphify/skill.md:114-151`). | Semantic indexing chunks text/code for retrieval, but it does not materialize document/media concepts and their typed relations into the callgraph (`crates/aft/src/semantic_index.rs:1895-1915`, `crates/aft/src/semantic_index.rs:4898-4927`). |

The principal precision difference is representational. Graphify unifies many useful relation types, but its default simple NetworkX graph and heuristic confidence tier answer “what relation survived between these concepts?” AFT stores each syntactic ref/callsite, explicit unresolved state, byte ranges, and resolver provenance; its navigation can therefore answer callsite-specific questions more precisely. Conversely, AFT has no answer to “what cluster explains concept X across docs, diagrams and code?” because those objects are not in one graph.

## 3. Edge provenance

### Are the taxonomies the same idea?

They overlap but are not equivalent.

- Graphify's `EXTRACTED` means the relationship is explicit in source; `INFERRED` means a deterministic or model pass inferred a relationship; `AMBIGUOUS` is retained uncertainty. Confidence score is a second axis within the tier (`graphify/graphify/llm.py:478-507`, `graphify/graphify/export.py:169-177`). Import-backed direct calls are `EXTRACTED`; unimported unique-name direct calls and callback-value links are `INFERRED`/0.85; Python imported-use edges are `INFERRED`/0.95 (`graphify/graphify/extract.py:7019-7083`, `graphify/graphify/extractors/resolution.py:2090-2108`).
- AFT provenance records *which resolver established a call target*. Stored refs have `status` plus `resolution_kind`; the global fallback writes `type_match` or `name_match`, and otherwise preserves unresolved rows (`crates/aft/src/callgraph_store/mod.rs:1914-1960`, `crates/aft/src/callgraph_store/mod.rs:13618-13776`). `type_match` and language-resolver/treesitter provenance can count as resolved for dead-code projection, while bare `name_match` does not (`crates/aft/src/callgraph_store/dead_code_projection.rs:351-359`).

Thus a Graphify `EXTRACTED` edge may still have required cross-file matching, while an AFT `type_match` says specifically how endpoint resolution happened. Graphify confidence is an evidence assertion across arbitrary relation types; AFT provenance is a resolver audit trail for callgraph edges.

### What reaches the agent?

Graphify renders the tier on every queried edge and in neighbor output, and renders inferred edges distinctly in HTML (`graphify/graphify/serve.py:1073-1089`, `graphify/graphify/serve.py:1832-1849`, `graphify/graphify/exporters/html.py:561-580`). AFT already carries provenance to its structured navigation adapters: call-tree nodes, caller entries, impact callers and trace hops all have optional `resolved_by`/`approximate` fields (`crates/aft/src/commands/callgraph_store_adapter.rs:73-98`, `crates/aft/src/commands/callgraph_store_adapter.rs:116-145`). However, the adapter intentionally supplies those fields only for supplemental `name_match`/`type_match`; exact resolver kinds remain absent (`crates/aft/src/callgraph_store/mod.rs:2023-2037`, `crates/aft/src/commands/callgraph_store_adapter.rs:275-281`). The public tool description simplifies this to `~` for name-only, `[unresolved]`, and “unmarked = exact” (`packages/opencode-plugin/src/tools/navigation.ts:31-43`).

### What AFT would change to show provenance per edge

No schema migration is needed: `refs.resolution_kind` and `StoreCallSite.provenance` already exist. A small engine change would make `edge_resolved_by()` return a normalized stable value for every edge rather than only `supplemental_resolution()`, preserving unresolved/local/direct cases; adapters already have fields to carry it. A bridge/renderer change would then print a compact marker or an opt-in `showProvenance` detail so common output does not grow on every hop. Finally, the tool description must enumerate the stable public vocabulary rather than exposing internal resolver strings. The seams are `crates/aft/src/callgraph_store/mod.rs:2010-2037`, `crates/aft/src/commands/callgraph_store_adapter.rs:73-145`, and the agent contract at `packages/opencode-plugin/src/tools/navigation.ts:31-43`.

## 4. Performance & scale

### Graphify's bounded work

| Stage | Bound / behavior |
|---|---|
| Discovery | Recursive walk with pre-descent ignore pruning and symlink-loop/out-of-root guards; the large-corpus thresholds (500 files / 500k words) affect warning text, not admission (`graphify/graphify/detect.py:51-53`, `graphify/graphify/detect.py:1798-1835`, `graphify/graphify/detect.py:1935-1965`). |
| AST extraction | Per-file parallelism uses process workers selected from explicit/env/CPU settings; Windows clamps to 61; a one-worker case runs serially; failures retry/fallback rather than impose a global timeout (`graphify/graphify/extract.py:5730-5815`). Source bytes are parsed per file; there is no total repo file/time cap in this code path. |
| Semantic extraction | Text units are capped/sliced at 20k characters; packing defaults to 60k estimated tokens; concurrency defaults to 4; truncation recursively splits at most the configured retry depth (default 3) (`graphify/graphify/llm.py:28-37`, `graphify/graphify/llm.py:2514-2589`). Raster image bytes are capped for inline backends (`graphify/graphify/llm.py:836-876`). |
| Resident model | Full node/link JSON is parsed and rehydrated as NetworkX; graph file load defaults to a configurable 512 MiB cap (`graphify/graphify/security.py:24-66`, `graphify/graphify/build.py:798-824`). This entails simultaneous Python dictionaries, NetworkX objects, and sometimes serialization buffers. |
| Clustering | Operates over the full undirected graph, with no sampling. Oversized/low-cohesion communities are repartitioned (`graphify/graphify/cluster.py:223-325`). |
| HTML | Full node view defaults to at most 5,000 nodes; supported callers may switch to a community meta-graph, otherwise export refuses (`graphify/graphify/exporters/html.py:14-30`, `graphify/graphify/exporters/html.py:396-479`). |
| Query | Seed scoring uses a lazily cached trigram postings map and may fall back to a whole-node scan when postings are not selective; traversal depth and hub expansion are bounded, and rendering is token-budgeted (`graphify/graphify/serve.py:369-442`, `graphify/graphify/serve.py:934-989`, `graphify/graphify/serve.py:992-1173`). |

The “no LLM for code” claim holds even for cross-file resolution: all generic and language-specific code resolvers are Python/tree-sitter passes over extracted facts (`graphify/graphify/extract.py:6613-7107`). It does not mean every edge is syntactically certain; name/path/type heuristics yield `INFERRED` edges, but they remain deterministic and zero-token (`graphify/graphify/extract.py:7019-7083`, `graphify/graphify/extract.py:7293-7301`).

### AFT comparison

AFT's cold callgraph build is explicitly admission-controlled. `ColdBuildLimiter` tracks total/per-stage deadlines, file limits, breaker-open state and staged resumability (`crates/aft/src/callgraph_store/mod.rs:1041-1119`). The cold builder scans/imports/node-extraction/ref-extraction in stages, writes progress offsets, commits partial coherent work, and returns `BuildPending` when bounded rather than finishing an unbounded corpus synchronously (`crates/aft/src/callgraph_store/mod.rs:2952-3077`, `crates/aft/src/callgraph_store/mod.rs:3088-3215`). Graphify's per-file cache reduces repeated parsing, but its clustering and in-memory materialization remain whole-graph costs; AFT's persisted store permits indexed SQL reads and file-level transactions.

For scale orientation, the AFT OSS matrix figures supplied for this investigation were: Rails, about 5,000 files, search 0.8 s and callgraph 397 s; Nx, about 10,700 files, search 1.7 s and callgraph 785 s. Both were measured before a known resolver-defect fix, so they should not be presented as current post-fix throughput. Graphify's `BENCHMARKS.md` has no comparable wall-clock values; its ERPNext case is 1,919 files / 27,843 nodes and reports quality/shape/cost, not build duration (`graphify/BENCHMARKS.md:124-157`). Any speed comparison would require running both at fixed commits on the same corpora, separating cold discovery/extraction/resolution/cluster/export from warm update/query.

## 5. Agent-surface design

### Graphify

The installed skill is a procedural controller, not just tool documentation. It tells an agent to check for `graphify-out/graph.json`, use query/explain/path rather than reading the full report for focused questions, and route initial versus update modes differently (`graphify/graphify/skill.md:43-78`). Generated always-on instructions tell the agent to read the graph report before architecture questions and run `graphify update .` after code edits (`graphify/AGENTS.md:3-8`). Optional hooks add a pre-read/pre-search nudge; strict mode may deny only the first raw read and then degrade to guidance (`graphify/graphify/cli.py:18-69`).

The query grammar has three useful layers:

1. natural-language lexical seeding (`graphify query "how does auth refresh work"`);
2. traversal controls (`--dfs`, context relation filters, token budget; MCP also exposes depth);
3. exact graph operations (`explain`, `path`, neighbor/node/community tools) for follow-up.

The result format is compact, source-located and direction-preserving. Seeds are rendered first, nodes are ordered by hop distance then degree, and truncation is announced at both top and bottom with exact shown/cut counts and narrowing hints (`graphify/graphify/serve.py:992-1147`). That is the strongest “token economics” idea here: the response admits incompleteness and tells the agent what to do next. One caveat is deliberate overrun: if every node fits but edges do not, Graphify returns the complete over-budget answer and says so (`graphify/graphify/serve.py:1101-1132`).

Staleness is workflow-visible rather than query-enforced. Reports record `built_at_commit` and tell the reader to compare HEAD; graph JSON can carry the commit (`graphify/graphify/report.py:172-179`, `graphify/graphify/export.py:402-410`). Hook logic can warn that a particular file changed after the graph build (`graphify/graphify/cli.py:42-50`). But the `query` command itself validates and loads the chosen JSON without comparing its commit to current HEAD (`graphify/graphify/cli.py:1244-1310`). The skill/update rule is therefore load-bearing.

### AFT

AFT's navigation description is operation-led: it distinguishes one-level calls in zoom from reverse/multi-level callgraph operations, maps user intents to `callers`, `impact`, `call_tree`, `trace_to`, `trace_to_symbol`, and `trace_data`, documents default depths, test hiding and unresolved collapsing, and tells the agent how to disambiguate a target (`packages/opencode-plugin/src/tools/navigation.ts:26-85`). This is more precise for code refactoring than a general graph question, but has no “explain concept X” entrypoint spanning docs and code.

AFT controls breadth structurally. Callers/impact turn into a hub summary after 20 total entries and return 15 representatives; trace prefixes and retained paths are capped (`crates/aft/src/commands/callgraph_store_adapter.rs:23-30`, `crates/aft/src/commands/callgraph_store_adapter.rs:479-505`). Outline caps output at 30 KiB and returns completeness flags (`crates/aft/src/commands/outline.rs:38-106`). Zoom's miss path computes a bounded menu of nearby outline symbols rather than dumping the file (`crates/aft/src/commands/zoom.rs:525-564`). Compared with Graphify, AFT has stronger operation semantics and persisted freshness machinery; Graphify has more explicit edge-by-edge confidence and truncation/narrowing language.

## 6. What to borrow

Size estimates: **S** is a focused change in existing response/rendering code; **M** crosses an engine and tool surface; **L** adds a durable subsystem or data model.

1. **Expose normalized provenance on every navigation edge (S).**
   - **What:** keep AFT's `~` shorthand, but optionally print a stable resolver provenance (`direct`, `local`, `type_match`, `name_match`, language resolver) on each hop/caller. Graphify demonstrates that confidence can remain compact and source-located in line-oriented output (`graphify/graphify/serve.py:1073-1089`, `graphify/graphify/serve.py:1832-1849`).
   - **AFT seam:** provenance already exists at `crates/aft/src/callgraph_store/mod.rs:2010-2037`; adapters already carry fields at `crates/aft/src/commands/callgraph_store_adapter.rs:73-145` but filter exact kinds at `crates/aft/src/commands/callgraph_store_adapter.rs:275-281`.
   - **Risk:** internal resolver strings become accidental API. Normalize to a versioned small enum before exposing them.

2. **Standardize explicit truncation envelopes and narrowing hints (S).**
   - **What:** every bounded navigation/outline response should state “showing X of Y,” put the warning before the data, and suggest the exact narrowing parameter. Graphify's renderer makes silent absence impossible (`graphify/graphify/serve.py:1091-1173`).
   - **AFT seam:** hub thresholds/limits at `crates/aft/src/commands/callgraph_store_adapter.rs:23-30` and caller summarization at `crates/aft/src/commands/callgraph_store_adapter.rs:479-505`; outline completeness at `crates/aft/src/commands/outline.rs:38-106`.
   - **Risk:** response verbosity. Use one bounded header/footer and preserve current compact summaries.

3. **Add a bounded “concept neighborhood” composition tool (M).**
   - **What:** natural-language lexical/semantic seed selection followed by a shallow union of callgraph relationships and existing semantic-search hits—an AFT-native equivalent of Graphify's `query_graph`, not a second graph store. Graphify's useful pieces are IDF/trigram seed scoring, per-term seed coverage, context filtering and hub-aware BFS (`graphify/graphify/serve.py:300-322`, `graphify/graphify/serve.py:369-442`, `graphify/graphify/serve.py:666-755`, `graphify/graphify/serve.py:934-962`).
   - **AFT seam:** lexical base+delta index at `crates/aft/src/search_index.rs:295-362`, semantic top-k at `crates/aft/src/semantic_index.rs:3485-3564`, and read-only graph APIs exposed by `crates/aft/src/callgraph_store/mod.rs:4745-4898`.
   - **Risk:** mixing relevance scores and call edges can look causal. Render sections separately and label why each seed/edge was included.

4. **Build an offline community projection over the persisted callgraph (M).**
   - **What:** compute deterministic subsystem clusters as a derived artifact, with stable IDs across updates and hub exclusion. Graphify's stable edge ordering, seeded Leiden/Louvain fallback, oversize splitting and overlap remapping are directly reusable design ideas (`graphify/graphify/cluster.py:96-172`, `graphify/graphify/cluster.py:223-325`, `graphify/graphify/cluster.py:361-409`).
   - **AFT seam:** read a generation through `ReadonlyCallGraphStore` (`crates/aft/src/callgraph_store/mod.rs:4745-4898`) and persist projection generation/revision alongside the existing store metadata (`crates/aft/src/callgraph_store/mod.rs:2080-2180`).
   - **Risk:** communities can imply architecture where shared utilities merely connect code. Keep them advisory, expose cohesion/inputs, and never feed them into resolution or dead-code truth.

5. **Offer a generated whole-graph HTML artifact (M).**
   - **What:** an opt-in static viewer for nodes, directed edges, provenance and derived communities, degrading to a community meta-graph above a configured limit. Graphify's 5,000-node guard and aggregation path avoid pretending browser physics scales indefinitely (`graphify/graphify/exporters/html.py:14-30`, `graphify/graphify/exporters/html.py:396-479`).
   - **AFT seam:** export from the read-only SQLite generation (`crates/aft/src/callgraph_store/mod.rs:2760-2816`) and reuse `resolution_kind`/source locations from refs (`crates/aft/src/callgraph_store/mod.rs:1914-1960`).
   - **Risk:** generated HTML can become a stale, very large artifact or create CSP/supply-chain concerns. Embed assets or pin integrity, stamp store generation/HEAD, and do not auto-generate on every edit.

6. **Stamp every agent answer with artifact identity/freshness (S).**
   - **What:** include cache generation/root/indexed count (and stale-file count when nonzero) in graph-derived answers, analogous to Graphify's graph path/node count header and report commit (`graphify/graphify/serve.py:1234-1251`, `graphify/graphify/report.py:172-179`).
   - **AFT seam:** read-only stores already expose generation, revision, currentness and stale files (`crates/aft/src/callgraph_store/mod.rs:4761-4777`).
   - **Risk:** repetitive noise and path leakage. Emit a short header only on non-current/borrowed/ambiguous-root cases; sanitize external paths.

7. **Explore non-code relation ingestion as a separate sidecar, not callgraph rows (L).**
   - **What:** let AFT semantic chunks optionally materialize cited concepts/decisions from Markdown/PDFs/images and connect them to code symbols, retaining evidence source and confidence. Graphify's semantic schema and evidence binding are concrete starting points (`graphify/graphify/llm.py:478-507`, `graphify/graphify/llm.py:637-690`, `graphify/graphify/llm.py:2005-2019`).
   - **AFT seam:** semantic chunk persistence at `crates/aft/src/semantic_index.rs:1895-1915` and search results at `crates/aft/src/semantic_index.rs:2185-2200`; code identities can be looked up read-only through `crates/aft/src/callgraph_store/mod.rs:4792-4806`.
   - **Risk:** model-derived edges can poison code-navigation truth. Keep a physically/logically separate store, require source quotes/hashes, and never let inferred semantic edges affect call resolution, refactoring, or dead-code reachability.

### What not to borrow

- **Do not replace SQLite with NetworkX/JSON as AFT's primary callgraph.** Graphify reconstructs the whole graph in memory (`graphify/graphify/build.py:798-824`); AFT depends on persisted callsite rows, read-only generations and indexed queries (`crates/aft/src/callgraph_store/mod.rs:1826-1960`, `crates/aft/src/callgraph_store/mod.rs:2452-2561`). A viewer/projection can be NetworkX-like without becoming the source of truth.
- **Do not collapse resolver provenance into a single confidence number.** Graphify's 0.85/0.95 ladder is appropriate across heterogeneous semantic relations (`graphify/graphify/extract.py:7019-7083`), but AFT's `type_match` versus `name_match` carries actionable failure semantics (`crates/aft/src/callgraph_store/mod.rs:13618-13776`). Preserve both resolver kind and any future calibrated confidence.
- **Do not make raw-read denial the default.** Graphify's strict hook intentionally denies one read and then softens (`graphify/graphify/cli.py:53-69`). AFT tools are accelerators; stale/incomplete indexes must not prevent source inspection.
- **Do not add LLM code-edge extraction.** Graphify itself keeps code out of its semantic model path (`graphify/graphify/llm.py:637-649`). AFT's refactor/dead-code consumers need deterministic, auditable call edges.
- **Do not generate a full HTML/community artifact on every watcher event.** Graphify's update can pay whole-graph cluster/export costs after incremental extraction (`graphify/graphify/watch.py:1305-1335`). AFT's live incremental path should remain bounded; derive visualizations explicitly or on idle snapshots.

## 7. Open questions for the operator

1. Should “concept neighborhood” be a new tool, or an `aft_search` mode that composes existing lexical/semantic results with one-hop callgraph context? The former is clearer; the latter avoids another agent-visible primitive.
2. Is the desired provenance contract diagnostic (show only approximate/unresolved edges, as today) or audit-grade (show the exact resolver for every edge)? Audit-grade output should be opt-in to avoid multiplying tokens.
3. Would an HTML/community export be used often enough to justify a maintained front end, or is a stable JSON/GraphML export for external viewers sufficient?
4. If non-code concepts are added, which evidence threshold is allowed to connect them to canonical code symbols, and who owns staleness when a PDF/image changes? This decision should precede schema work because those edges must not leak into dead-code or refactoring truth.
5. Should a comparative benchmark be added to the OSS matrix that records cold build stage timings, warm changed-file refresh, resident RSS, query p50/p95, resolver precision/recall, and answer tokens for both systems at pinned commits? Current published figures are not methodologically comparable.
