# ripwire vs AFT (2026-09)

Scope: `redhat-et/ripwire` at `6488f6f48af723e01e14d25a2e9ec7c5e49daafc` (2026-09-06) versus AFT at `47226642ea16863b398a59e9539a8b12b10ddb78`. Ripwire citations are relative to that pinned checkout; AFT citations are relative to this repository.

## Summary

- Ripwire is a local single-binary parser/graph/context tool; AFT splits search, persistent call-graph navigation, and inspection across tool calls (`ripwire/README.md:14-30`; `packages/opencode-plugin/src/tools/navigation.ts:27-47`).
- **Ranking:** default task rank is routed lexical BM25 plus mention boosts, not PageRank or risk metrics; it won 3/5 microcases but identifier-light paraphrases exposed vocabulary dependence (`ripwire/src/lexical.h:3-7`).
- **Risk rows:** the compact shape is worth borrowing, but `amp` is direct callers plus file co-change partners—not a transitive “nodes feel it” count (`ripwire/src/main.cpp:3594-3598`).
- **Determinism/cache:** no daemon does not mean stateless: per-root parse blobs, per-file gates, HEAD-keyed quality blobs, and history memos preserve dirty parses while accelerating warm calls (`ripwire/src/ingest_prewarm.h:165-204`).
- **Budget:** 4.3K tokens is an observation, not a fixed cap; rank-tiered doc/signature trimming, row dropping, and a disclosure ladder enforce byte-derived budgets (`ripwire/src/serialize.h:684-722`).
- **Activation:** moment-based “when to reach for it” routing is worth adopting; AFT already does this in tool descriptions and should add one small knowhow router (`ripwire/skills/ripwire-router/SKILL.md:15-60`).
- **Languages:** Ripwire adds Metal/CUDA and TOML to its 21-grammar surface, but AFT supports more language families and exposes edge provenance more clearly (`ripwire/src/ingest_crawl.h:60-152`; `crates/aft/src/parser.rs:658-733`).
- **Bench/paper:** adopt repository-disjoint LocBench with strict all-file scoring; no committed harness has an AFT arm, so no published Ripwire number compares AFT (`ripwire/bench/headtohead/r4-2026-08-06/r4_worker.py:296-307`).

## 1. Ranking model: task phrase to symbols without embeddings

### What actually ranks

`--for` first classifies the query. A camel/snake identifier in a query of at most two content words, or a query whose every content word names an existing symbol, selects whole-name BM25; ordinary multi-word prose selects subtoken/body BM25. Common plain-name anchors can be declined using corpus-derived definition and carrier counts (`ripwire/src/lexical.h:1850-1970`). The conceptual scorer's per-symbol document contains name subtokens, callee names, doc comments, and body text; Ripwire explicitly says it does **not** fuse PageRank because its evaluation found importance harmed relevance (`ripwire/src/lexical.h:3-7`). Weighted term frequencies are prepared at parse/cache time and reused without rereading or retokenizing the corpus (`ripwire/src/lexical.h:529-539`); query-time BM25 uses the standard IDF and length-normalized saturation sum (`ripwire/src/lexical.h:1082-1108`).

The default combination is therefore:

1. query routing to whole-name BM25 or subtoken/body BM25;
2. deterministic tier multipliers for generated/test/doc material;
3. literal file/module/symbol mention lifts;
4. same-graph Markdown backtick mention lifts.

The graph-anchor, sibling, file-pooling, structural-expansion, and co-change-prior stages are experimental or opt-in, while query-mention and doc-mention lifting are default-on (`ripwire/src/verbs_for.h:128-239`). Generated/test/doc tier multipliers are applied before those lifts (`ripwire/src/verbs_for.h:85-112`). Complexity, churn, purity, test reach, and amplification decorate the selected rows later; they are not terms in the default rank.

`confidence` is also not a learned probability. Ripwire sorts positive scores, finds the largest relative adjacent drop, and treats a drop of at least 20% inside the return window as a relevance cliff (`ripwire/src/lexical.h:2001-2082`). `high` means either such a cliff was found or the served set contains every positive hit; otherwise it is `low`. `margin_pct` is the in-window relative drop, or zero for a flat/capped head (`ripwire/src/lexical.h:2085-2108`). The `--for` path computes the same statistic over the full positive distribution with floor 5 and the configured/default ceiling, then derives disclosure after removing zero-score padding (`ripwire/src/verbs_for.h:1535-1598`).

### AFT comparison

AFT's natural-language lane embeds the query and searches a pre-embedded symbol index (`crates/aft/src/commands/semantic_search.rs:3187-3253`, `crates/aft/src/semantic_index.rs:3491-3494`), obtains a separately trigram-ranked file list, and fuses ranks. Its E1 exact phrase tier reads and normalizes the candidate file; its E2 tier accepts all normalized content tokens within a one-to-three-line window (`crates/aft/src/commands/semantic_search.rs:1360-1392`). Exact phrases sort ahead of exact windows, then fusion score, then lane score (`crates/aft/src/commands/semantic_search.rs:3429-3474`). For ordinary natural language, RRF is overwhelmingly semantic (`0.999`) with lexical corroboration (`0.001`), using `1/(60+rank+1)` per lane (`crates/aft/src/commands/semantic_search.rs:41`, `crates/aft/src/commands/semantic_search.rs:3604-3702`). AFT then adds one-hop direct-caller annotations only when a ready call-graph view is already available (`crates/aft/src/commands/semantic_search.rs:4373-4431`); `impact` separately walks reverse callers recursively (`crates/aft/src/commands/callgraph_store_adapter.rs:559-600`).

Thus the systems optimize different failure modes: Ripwire can score immediately without a model and can match behavior terms found in bodies, while AFT has a semantic lane for paraphrase. Neither microcase below supports a broad quality claim.

### Five-task measurement

Expected target was chosen before looking at either result. “Hit” means the first result is the implementation target, not merely a nearby type, test, constant, or formatter.

| Repo | Task phrase | Expected implementation | Ripwire first | AFT first | First-result verdict |
|---|---|---|---|---|---|
| `tmignore-rs` | hot reload configuration after file system changes | `handle_event` / `reload_file` | `handle_event`, `src/commands/monitor.rs:36` | `reload_file`, `src/config.rs:202` | both hit |
| `tmignore-rs` | find gitignored files without executing repository config hooks | `find_ignored_files` | `find_ignored_files`, `src/git.rs:88` | `find_ignored_files`, `src/git.rs:88` | both hit |
| `tmignore-rs` | migrate legacy cache on startup | `import_legacy_cache_file` | `import_legacy_cache_file`, `src/main.rs:251` | `LegacyCache`, `src/legacy_cache.rs:5` | Ripwire hit; AFT nearby data type |
| AFT | combine semantic and lexical search results while preferring exact phrases | `fuse_hybrid_results_with_zoom` | `BORROWED_SEMANTIC_LOADING_WITH_LEXICAL_RESULTS`, `semantic_search.rs:90` | `semantic_index::search`, `semantic_index.rs:3491` | both miss the fusion implementation |
| AFT | compute transitive blast radius for a symbol and summarize hubs | `impact_result` | `blast_radius_annotations`, `semantic_search.rs:4373` | benchmark `RankedResult`, `benchmarks/codegraph-vs-aft-retrieval/src/types.ts:36` | Ripwire nearby annotation; AFT miss |

On this deliberately tiny sample, Ripwire put the exact implementation first in 3/5 tasks and AFT in 2/5. Ripwire routed all five through `subtoken+body`, so this is not a whole-name lookup test.

Identifier-light paraphrases reused the same two command shapes and five-result cap. For “refresh settings when their file changes,” “discover paths ignored by version control without trusting local hooks,” and “convert old saved state when the application begins,” Ripwire's first results were respectively `Commands`, `stats::execute`, and `Commands`; AFT's were `reset::execute`, the relevant hook-safety **test**, and `reset::execute`. Neither system put the intended implementation first in any of the three paraphrases. AFT still placed `reload_file` second and `find_ignored_files` fifth; Ripwire placed `handle_event` third but the other targets outside its top five. The fair conclusion is narrower than either product story: Ripwire was competitive when task words overlapped names/docs/bodies, and degraded sharply when those words were replaced; embeddings improved the tail in two cases but did not make AFT's rank one robust.

Commands (the table contains every `ROOT`/`TASK` substitution; no omitted flags):

```sh
# Ripwire, one invocation per row
/Users/ufukaltinok/Work/OSS/ripwire/build-release/ripwire ROOT \
  --for='TASK' --format=candidates --top-k=5

# AFT raw protocol, driven by benchmarks/aft-search/run.py:AftClient.
# One process was configured per repo; these are the exact request shapes.
configure({"project_root":"ROOT","harness":"opencode","storage_dir":"/tmp/aft-ripwire-q1-REPO","config":[{"tier":"user","source":"<aft-search-benchmark>","doc":"{\"search_index\": true, \"semantic_search\": true}"}]})
semantic_search({"query":"TASK","top_k":5})
impact({"file":"FIRST_NAMED_RESULT_FILE","symbol":"FIRST_NAMED_RESULT_NAME","depth":5})

# The two AFT-repository searches were the registered tool itself:
aft_search({"query":"TASK","topK":5,"includeTests":false})
```

The raw client waits for both trigram and semantic indexes before evaluating (`benchmarks/aft-search/run.py:69-115`). Explicit `impact` follow-ups were run for the three `tmignore-rs` leaders; current-repository `aft_search` instead exercised its ready-store direct-caller suffix. The three impact calls reported 9 affected sites for `reload_file`, 14 for `find_ignored_files`, and zero for the `LegacyCache` type; those are call-graph results, not search-score inputs.

## 2. Risk annotations in place

Ripwire's row shape is genuinely useful: one ranked `<d>` can carry cyclomatic/cognitive complexity, fan-in, recent file churn, amplification, `tested`, and syntactic purity. The implementation computes these after ingest and graph construction whenever `--for`, `--metrics`, or `--exemplar` is active (`ripwire/src/main.cpp:3580-3617`). Specifically:

- `tested=1` means a symbol is transitively reachable from a syntactically identified test symbol over resolved outgoing calls; dynamic dispatch, unbound callbacks, and subprocess tests remain outside that set (`ripwire/src/graph.h:3813-3849`).
- purity is a least fixpoint: calls to a small side-effecting intrinsic set seed impurity, which propagates backward to callers over the incoming CSR (`ripwire/src/graph.h:3745-3800`).
- churn is the count of commits touching the symbol's file in a 12-month subwindow, produced from the same 18-month history walk used for co-change (`ripwire/src/main.cpp:3608-3665`). The history stream is keyed by repository, HEAD, window text, moving boundary SHA, and schema; a warm hit skips the `git log --name-only` walk (`ripwire/src/quality.h:2826-2869`).
- `amp` is **not** transitive impact. It is `direct caller count + number of distinct files that co-changed with this symbol's file` (`ripwire/src/main.cpp:3594-3598`, `ripwire/src/main.cpp:3667-3693`). That mixed-granularity definition conflicts with README prose saying that `amp=266` means 266 graph nodes “feel” a change (`ripwire/README.md:42-49`). The compact row is worth borrowing; that interpretation is not.

These are query-time computations over the warm parsed facts and freshly built in-memory graph. Parse-time stores weighted lexical statistics (`ripwire/src/lexical.h:529-539`), but each process still builds the graph and computes QMetrics; only file parsing and the committed history walk are memoized (`ripwire/src/main.cpp:3470-3503`, `ripwire/src/main.cpp:3560-3564`).

AFT currently separates the same questions. Complexity is a parallel tree-sitter inspection category with threshold 10 (`crates/aft/src/inspect/scanners/complexity.rs:1-25`). `impact` traverses and counts affected call sites, filters test-origin sites by caller path, and switches to a hub summary over its threshold (`crates/aft/src/commands/callgraph_store_adapter.rs:590-639`); test origin itself is currently the path predicate `is_test_file` (`crates/aft/src/commands/callgraph_store_adapter.rs:354-359`). Search's only in-row graph enrichment is the compact `↩N basename,…` direct-caller suffix (`crates/aft/src/commands/semantic_search.rs:4396-4431`).

**Verdict:** extend AFT navigation/search rows with cheap facts already resident in the relevant view: direct caller count, exact/approximate edge provenance, and—when inspect has a fresh result—complexity. Preserve “absent means unavailable,” not zero. Do not import Ripwire's `amp` label; AFT's transitive `total_affected` and direct `↩N` should remain separately named. This advances the self-contained/truncation-envelope direction without forcing every search to synchronously run inspection or Git history.

## 3. Determinism, no daemon, and cache design

“No daemon” does not mean “re-index every byte.” The ordinary auto-cache path is stable per realpath root and split into rich (`--for`, metrics, uses, exemplar) and lean verb classes (`ripwire/src/main.cpp:160-200`). Its blob is a deterministic superset of per-file records; each record carries a root-relative path, content hash, size/mtime/ctime gates, parse health, and extracted facts, while an offset table permits selective reads and carry-forward of files excluded by the current run (`ripwire/src/ingest_cache.h:799-851`). The header's magic, cache version, parser version, architecture, checksums, and exact-fit checks reject stale or torn blobs (`ripwire/src/ingest_cache.h:1001-1064`).

The named caches serve different identities:

- `kCacheMagic` identifies the ordinary per-root incremental extraction blob (`ripwire/src/ingest_cache.h:106`, `ripwire/src/ingest_cache.h:1001-1021`); only exact size/mtime/ctime matches older than the cache write bypass reading, while every mismatch falls through to read-and-hash, so uncommitted edits are included (`ripwire/src/ingest_prewarm.h:165-207`).
- `headSnapRepoHex` is an FNV hash of the root's realpath: repository namespace, not source-content identity (`ripwire/src/quality.h:1356-1369`). `shaKeyedCachePath` builds sharded family files from repository/config/SHA hashes (`ripwire/src/quality.h:1616-1633`). HEAD-snapshot quality baselines include canonical root, HEAD SHA, excludes, and extraction identity and then revalidate every file by content hash (`ripwire/src/quality.h:1332-1350`).
- `spanTierMemoPath` is path-identity keyed and only for files at least 32 KiB; load requires the source to be provably older than the memo and exact size/mtime/ctime/path equality (`ripwire/src/ingest_astquery.h:1280-1368`).
- history churn is intentionally committed-history-only, but resolves the cached raw history stream against the current live ingest, so dirty additions/removals do not make path mapping stale (`ripwire/src/quality.h:2709-2717`).

AFT's v3 content-addressed-view design goes further: immutable per-file blobs plus a checkout-specific manifest/view; call-graph blobs hold unresolved extraction, and cross-file resolution becomes a deterministic view join (`docs/design/content-addressed-index-views.md:25-57`). Clean files can map Git OIDs to BLAKE3 without reading bytes, while dirty/untracked files hash working-tree bytes (`docs/design/content-addressed-index-views.md:64-76`). That architecture preserves a daemon's standing views while gaining Ripwire-like content reuse across branches and worktrees.

### Cold/warm measurement

Rails had 4,996 tracked files by `git ls-files`; Ripwire's report admitted 3,916 files, 60,438 symbols, and 102,883 edges. Both binaries were optimized (`cargo --release`; Ripwire `CMAKE_BUILD_TYPE=Release`). Times are wall clock on this machine, one cold then one warm run.

| Surface | Cold | Warm | What completed |
|---|---:|---:|---|
| Ripwire `--for`, isolated empty cache | 1.02 s | 0.32 s | ranked top five from 60,438 indexed symbols |
| AFT callgraph + `impact`, isolated empty store | 290.558 s | 0.251 s | durable SQLite callgraph became queryable, then `impact(create_or_update)` returned |

Exact commands:

```sh
# Build modes
cargo build --release -p agent-file-tools --bin aft
cmake -S /Users/ufukaltinok/Work/OSS/ripwire \
  -B /Users/ufukaltinok/Work/OSS/ripwire/build-release \
  -DCMAKE_BUILD_TYPE=Release
cmake --build /Users/ufukaltinok/Work/OSS/ripwire/build-release -j2

# Corpus count
python3 - <<'PY'
import subprocess
p='/Users/ufukaltinok/Work/OSS/rails'
print(subprocess.check_output(['git','-C',p,'ls-files','-z']).count(b'\0'))
PY

# Indexed corpus counts quoted above
XDG_CACHE_HOME=/tmp/ripwire-aft-q3-cache-release \
  /Users/ufukaltinok/Work/OSS/ripwire/build-release/ripwire \
  /Users/ufukaltinok/Work/OSS/rails --report

# Ripwire cold/warm; /usr/bin/time -p output was captured separately.
rm -rf /tmp/ripwire-aft-q3-cache-release
mkdir -p /tmp/ripwire-aft-q3-cache-release
XDG_CACHE_HOME=/tmp/ripwire-aft-q3-cache-release /usr/bin/time -p \
  /Users/ufukaltinok/Work/OSS/ripwire/build-release/ripwire \
  /Users/ufukaltinok/Work/OSS/rails \
  --for='find where Active Record persists a model and runs callbacks' \
  --format=candidates --top-k=5
# Repeat the preceding timed command unchanged for warm.

# AFT cold/warm, including the readiness loop.
rm -rf /tmp/aft-ripwire-q3-rails-release
mkdir -p /tmp/aft-ripwire-q3-rails-release
PYTHONPATH=benchmarks/aft-search python3 - <<'PY'
import json,time
from pathlib import Path
from run import AftClient
binary=Path('target/release/aft').resolve()
repo=Path('/Users/ufukaltinok/Work/OSS/rails')
storage=Path('/tmp/aft-ripwire-q3-rails-release')
params={'project_root':str(repo),'harness':'opencode','storage_dir':str(storage),
        'config':[{'tier':'user','source':'<ripwire-investigation>',
                   'doc':json.dumps({'search_index':False,'semantic_search':False})}]}
for mode in ('cold','warm'):
    started=time.perf_counter()
    client=AftClient(binary,repo,900,storage_dir=storage,semantic_search=False)
    attempts=0; last=None
    try:
        configured=client.call('configure',params,timeout_secs=60)
        while time.perf_counter()-started < 900:
            attempts+=1
            last=client.call('impact',{'file':'activerecord/lib/active_record/callbacks.rb',
                             'symbol':'create_or_update','depth':5},timeout_secs=180)
            if last.get('success') and 'total_affected' in last: break
            time.sleep(.5)
        print(json.dumps({'mode':mode,'elapsed_ms':round((time.perf_counter()-started)*1000,3),
              'attempts':attempts,'configured':configured.get('success'),'impact':last},separators=(',',':')))
    finally:
        client.close()
PY
```

This is not an equal-work speed benchmark. Ripwire rebuilt a disposable in-memory graph and answered one lexical task; AFT materialized a reusable, generation-swapped SQLite graph before answering. The AFT cold probe also issued 575 non-blocking readiness requests, so the wall number includes polling overhead. It does establish the product trade: warm request costs converge, while AFT's current durable cold join is orders of magnitude costlier on this corpus. AFT already describes the cold build as disk-backed inventory, bounded extraction batches, then staged reference resolution (`crates/aft/src/callgraph_store/mod.rs:3695-3705`), and its own design records the resolution stage as the expensive reason to share extraction (`docs/design/content-addressed-index-views.md:9-23`).

## 4. Token budgeting and what gets dropped

The README's “about 4.3K tokens” is a dated observation for one query, not the implementation's invariant (`ripwire/README.md:38-50`). In our default conceptual query, Ripwire emitted 8,399 bytes and declared `est_tokens="3360"`; the response kept 18 of 40 ranked signatures, 24 of 1,279 tail files, and 2 of 6 hop rows. Command:

```sh
XDG_CACHE_HOME=/tmp/ripwire-aft-q2-cache \
  /Users/ufukaltinok/Work/OSS/ripwire/build-release/ripwire . \
  --for='compute transitive blast radius for a symbol and summarize hubs' \
  > /tmp/ripwire-aft-q2.xml
```

Ripwire avoids pretending it knows the target agent's tokenizer. It calibrates map bytes/token by language, uses 2.36 B/token as the densest conservative rate, and reserves 10% headroom (`ripwire/src/serialize.h:512-601`). The default ranked-signature payload is 7,500 bytes; named-symbol auto mode can add a 6,000-byte body allowance, while an explicit token budget becomes a hard shared ceiling (`ripwire/src/serialize.h:699-756`). Conceptual routing intentionally uses a compact no-body shape and spends the remainder on one-hop edges (`ripwire/src/verbs_for.h:511-525`).

The drop order is explicit. Before pressure, ranks 1–12 may carry full doc excerpts, 13–24 capped excerpts, and the tail signatures only (`ripwire/src/serialize.h:684-697`). Under pressure the shared XML/JSON ladder shrinks tail signatures, drops rank 13–24 docs, caps then drops head docs, caps signatures, and finally drops whole entries from the tail while always preserving ranks 1–4 (`ripwire/src/serialize.h:3272-3355`). If the envelope itself is too large, a second ladder first removes the duplicate task echo, then the unique route attribute, and finally restores the full header with an honest over-ceiling marker rather than silently mutilating it (`ripwire/src/serialize.h:646-681`). What disappears is therefore observable through `shown/total/capped`, `dropped_positive`, `bodies/reason`, and tail/hop counts.

AFT's corresponding controls are coarser. `impact` changes to a deduplicated 20-entry hub summary when the affected set crosses its threshold, preserving total/hidden/shown/limit metadata (`crates/aft/src/commands/callgraph_store_adapter.rs:590-639`). `outline` uses a 30 KiB output cap and a narrowing hint (`crates/aft/src/commands/outline.rs:39-50`). Search separately caps rank-zero automatic full-symbol expansion at 250 lines (`crates/aft/src/commands/semantic_search.rs:92-106`).

**Verdict:** borrow Ripwire's machine-readable truncation envelope and explicit drop reason, not its XML or one universal token estimate. AFT's next step should be a common response budget object—`total`, `shown`, `dropped`, `reason`, and whether counts are exact/lower-bound—used by search, impact, outline, and trace renderers.

## 5. Skills and activation

The release installer stages versioned skills and then detects agent homes. At this commit it auto-activates only Claude Code (`~/.claude`) and Codex/Agents (`~/.codex` or `~/.agents`), using symlink installation and degrading to printed manual commands on failure; hooks remain opt-in because they capture command/path data (`ripwire/scripts/install.sh:199-260`). That is sensible consent behavior, but narrower than the README's “every agent it finds” list that also names Cursor, Windsurf, Gemini, opencode, and aider (`ripwire/README.md:23-30`).

The skill content is stronger than a flag catalog:

- `ripwire-orient` fires on cold arrival, “how does X work,” or post-compaction recovery, says to stop at the first sufficient rung, and chooses `--for` only after recall/report when appropriate (`ripwire/skills/ripwire-orient/SKILL.md:3-17`, `ripwire/skills/ripwire-orient/SKILL.md:37-76`).
- `ripwire-navigate` distinguishes one-hop callers from safety/blast-radius questions and tells the agent not to stack callers, callees, and impact ritualistically (`ripwire/skills/ripwire-navigate/SKILL.md:3-14`, `ripwire/skills/ripwire-navigate/SKILL.md:30-47`).
- `ripwire-router` maps recognizable moments—cold start, understanding, planning, pre-write reuse, mid-implementation, diff review, debugging, testing, and handoff—to one opening move (`ripwire/skills/ripwire-router/SKILL.md:15-60`). It also targets default reflexes such as whole-file Read or conceptual grep, not only named workflows (`ripwire/skills/ripwire-router/SKILL.md:65-87`).

`prompts/improve-for-my-language.md` is a transcript self-audit, not a generic feedback prompt. It requires every finding to identify the ask, command, and result; separates discoverability from capability; classifies grammar/symbol/resolution/ranking/disclosure failures; and requires ranking fixes to land as held-out cases rather than hand-inspected top tens (`ripwire/prompts/improve-for-my-language.md:1-64`).

AFT has already adopted the core framing. `aft_search` says when to use full natural-language phrasing versus terse exact input (`packages/opencode-plugin/src/tools/semantic.ts:32-65`). `aft_callgraph` says “reach for this whenever the question is about how symbols connect,” distinguishes one-level zoom from reverse/multi-level traversal, and gives an intent sentence for every operation (`packages/opencode-plugin/src/tools/navigation.ts:27-47`).

**Verdict:** add a concise moment router to AFT knowhow and use transcript-backed discoverability review, but do not mirror Ripwire's many overlapping installed skills. The highest-value additions are “before signature change → callers,” “risky edit → impact,” “post-compaction → search then targeted zoom,” and “stop after one unambiguous answer.” Keep host activation centralized in AFT's existing plugin setup rather than writing into every detected agent home.

## 6. Grammar table and cross-file references

Ripwire's declarative table has 40 extension rows over 21 vendored grammars. Each row chooses extension, logical language, grammar function, and embedded tags query (`ripwire/src/ingest_crawl.h:43-60`). Notable extra participation relative to AFT is:

- `.metal` uses the C++ grammar/query and stays in the C++ namespace;
- `.cu`/`.cuh` use tree-sitter-cuda but also stay logically C++;
- TOML tables/keys become data-section symbols;
- Markdown headings become section symbols and backtick links become doc-to-code mentions outside the call graph (`ripwire/src/ingest_crawl.h:61-152`, `ripwire/src/graph.h:2274-2310`).

AFT's parser enumerates 31 logical languages and extensions, including Bash and broad Markdown variants, but no Metal, CUDA, or TOML (`crates/aft/src/parser.rs:658-733`). It has several families Ripwire lacks: Zig, HTML, Solidity, SCSS, Vue, Scala, Kotlin, Perl, Pascal, R, and Groovy (`crates/aft/src/parser.rs:735-803`). Bash and Markdown are therefore not missing from AFT's search/outline parser; however, AFT's call extractor explicitly returns no call-node kinds for Bash, Markdown, ObjC, and several data/UI languages (`crates/aft/src/calls.rs:20-59`). Ripwire's Markdown links are likewise kept out of PageRank/blast radius, which is a good semantic boundary rather than a coverage defect (`ripwire/src/graph.h:2274-2277`).

Ripwire resolves without a typechecker using a deterministic evidence ladder: canonical/receiver narrowing, unique included/imported file, same-file, same-directory, then unique compatible global candidate; unresolved ambiguity is dropped rather than sprayed (`ripwire/src/graph.h:1628-1811`). It can further prune ambiguous method candidates by receiver class hierarchy and arity, and an optional SCIP overlay marks compiler-pinned edges (`ripwire/src/graph.h:1820-1842`, `ripwire/src/graph.h:39-64`). The default edge provenance value still collapses “unique name” and several syntactic narrowing routes into the broad name-based class; `amb` is a per-source count, not a per-edge `type_match` label.

AFT makes that distinction more directly. Method-dispatch edges with an inferred receiver type are inserted as `type_match`; only unknown receivers fall through denylisting and scored `name_match` selection (`crates/aft/src/callgraph_store/mod.rs:11583-11652`). Type selection requires a unique scoped candidate (`crates/aft/src/callgraph_store/mod.rs:13318-13344`), while fallback name selection uses receiver-word overlap plus path proximity and rejects ties/low scores (`crates/aft/src/callgraph_store/mod.rs:13346-13421`). Navigation exposes name-only edges as approximate (`crates/aft/src/commands/callgraph_store_adapter.rs:48-52`).

**Verdict:** borrow Metal/CUDA routing and TOML section extraction if demand data justifies three new AFT lanes. Do not borrow Ripwire's coarser provenance; AFT's `type_match`/`name_match` distinction is the better contract. For Bash and Markdown, first decide whether graph semantics are meaningful: headings and doc links belong in retrieval/mention planes, while shell command edges need an explicit dynamic/external floor before appearing as ordinary calls.

## 7. Benchmarks and paper

The paper is a working preprint that warns its numbers are binary/version coupled and that external systems were not independently rerun (`ripwire/paper/PREPRINT.md:5-16`). Its strongest reusable methodology is not a headline percentage but the protocol: train-only calibration, one-shot repository-disjoint held-out evaluation, repository-clustered bootstrap, and a preregistered quality/cost gate (`ripwire/paper/PREPRINT.md:193-223`).

The committed benchmark families answer different questions:

- LocBench uses issue text (first 1,200 normalized characters) to rank patch-touched Python files/functions at the base commit, with a repository-disjoint 317/243 split and strict all-gold plus any-gold/MRR views (`ripwire/bench/locbench/README.md:19-39`, `ripwire/bench/locbench/README.md:49-90`). Its three arms compare Ripwire's own `--for`, pure lexical `--query`, and experimental graph-anchored `--for`; grep is not the baseline (`ripwire/bench/locbench/README.md:61-67`).
- Multi-SWE C++ uses linked issue title/body against human-verified C++ pull requests and scores non-added touched files; it reports single-file and harder multi-file results separately (`ripwire/bench/multiswe/README.md:12-55`, `ripwire/bench/multiswe/README.md:136-168`).
- The R4 head-to-head normalizes five competitors under one file-ranking evaluator, but its fixed arm choices are CodeSeek, RepoWise, codebase-memory-mcp, Graphify, and Aider—no AFT (`ripwire/bench/headtohead/r4-2026-08-06/r4_worker.py:296-307`).
- Agentloop is the more realistic end-to-end design: same agent runner/model with baseline, Ripwire CLI, and Ripwire-skills arms, reporting task resolution, localization, tokens, time, and cost; its own documentation cautions that the initial repository count is underpowered for small outcome differences (`ripwire/bench/agentloop/README.md:27-72`, `ripwire/bench/agentloop/README.md:74-141`).

AFT does have a reusable external-search evaluator, but on Vera-shaped tasks rather than Ripwire's corpus. It projects the first ten `aft_search` rows to file/range/name/kind and scores configurable exact-file or line-overlap relevance (`benchmarks/aft-search/run_external.py:93-138`, `benchmarks/aft-search/metrics.py:62-87`). It creates an isolated store, waits for indexes, and measures each query (`benchmarks/aft-search/run_external.py:162-180`). Those metric primitives can host a LocBench adapter, but existing AFT and Ripwire published numbers are not side by side.

### Same-task harness run

I ran one frozen held-out LocBench instance, `ultralytics__ultralytics-17810` at base commit `d8c43874ae830a36d2adeac4a44a8ce5697e972c`, then sent AFT the harness's exact normalized 1,200-character query against the same checkout. Its one primary gold file was `ultralytics/utils/ops.py` and gold function was `segment2box`.

| Surface | Exact file@10 | First gold-file rank | Gold function | Warm query wall | Setup/index | Emitted answer |
|---|---:|---:|---|---:|---:|---:|
| Ripwire `--for` | 0 | 11 | not returned | 0.2059 s | not requested in this run | 7,001 bytes; conservative ceiling 2,967 tokens |
| AFT `semantic_search(top_k=10)` | 0 | not in top 10 | not returned | 0.3128 s | 27.936 s cold; 1.144 s warm process/index load | 1,914 rendered-text bytes |

This row is comparable for **same query, checkout, exact-file truth, and file@10**. It is not enough for an accuracy claim, each latency is one sample, and the answer shapes differ: Ripwire's production bundle considered 40 symbols and placed the file at 11, while AFT was asked for ten symbols. It demonstrates that the published harness can host AFT and that this first case was a miss for both, not which system is better.

Exact commands:

```sh
rm -rf /tmp/ripwire-aft-locbench-one
mkdir -p /tmp/ripwire-aft-locbench-one
RIPWIRE=/Users/ufukaltinok/Work/OSS/ripwire/build-release/ripwire \
  python3 /Users/ufukaltinok/Work/OSS/ripwire/bench/locbench/run_locbench.py \
  --work-dir /tmp/ripwire-aft-locbench-one --arms for --split heldout \
  --max-scored 1 --latency-samples 1 --history-depth 1 \
  --json-out /tmp/ripwire-aft-locbench-one/ripwire.json --verbose

rm -rf /tmp/aft-ripwire-locbench-one-store-repro
PYTHONPATH=benchmarks/aft-search python3 - <<'PY'
import json, time
from pathlib import Path
from run import AftClient
rows=json.load(open('/tmp/ripwire-aft-locbench-one/datasets/rows_czlll__Loc-Bench_V1_test_25.json'))
row=next(x for x in rows if x['instance_id']=='ultralytics__ultralytics-17810')
query=' '.join(row['problem_statement'].split())[:1200]
repo=Path('/tmp/ripwire-aft-locbench-one/repos/ultralytics__ultralytics')
store=Path('/tmp/aft-ripwire-locbench-one-store-repro')
for mode in ('cold','warm'):
    client=AftClient(Path('target/release/aft').resolve(),repo,900,storage_dir=store,semantic_search=True)
    started=time.perf_counter()
    try:
        client.configure(); client.wait_for_indexes(require_search=True)
        ready=time.perf_counter(); response,latency=client.semantic_search(query,10)
        results=response.get('results') or []
        ranks=[]
        for rank,result in enumerate(results[:10],1):
            path=str(result.get('file','')).replace('/private/tmp/','/tmp/')
            if path.endswith('/ultralytics/utils/ops.py'): ranks.append(rank)
        print({'mode':mode,'setup_ms':round((ready-started)*1000,3),
               'query_ms':round(latency,3),'gold_file_first_rank':min(ranks) if ranks else None,
               'text_bytes':len((response.get('text') or '').encode()),'result_count':len(results)})
    finally:
        client.close()
PY
```

### Harness probes and comparability

| Command run | Result | Comparable AFT measurement? |
|---|---|---|
| `python3 /Users/ufukaltinok/Work/OSS/ripwire/bench/locbench/run_locbench.py --help` | Runnable driver exposes dataset, held-out split, warm samples, cold, and index timing; requires an external work directory/corpus clones. | Yes through the manual adapter above; not a committed arm. |
| `python3 /Users/ufukaltinok/Work/OSS/ripwire/bench/headtohead/r4-2026-08-06/r4_worker.py --help` | Failed with `ModuleNotFoundError: run_locbench` from the worktree cwd. | No. |
| `PYTHONPATH=/Users/ufukaltinok/Work/OSS/ripwire/bench/locbench python3 /Users/ufukaltinok/Work/OSS/ripwire/bench/headtohead/r4-2026-08-06/r4_worker.py --help` | Succeeded; accepted arms were exactly `codeseek, repowise, cbm, graphify, aider`. | No AFT arm. |
| Q1 five-task commands above | Same live snapshots, same task text, both tools, first result recorded. | Yes, but only a microcase—not benchmark evidence. |

I did not manufacture a cross-tool headline by comparing Ripwire's stored LocBench percentages to AFT's Vera results: corpora, truth granularity, setup accounting, and query surfaces differ. The next valid experiment is to freeze an AFT adapter before scoring, run both tools over all held-out repositories at the same base commits, report strict **all-patch** file@1/3/5/10 and any-file/MRR, and keep setup/index time separate from warm query time. AFT's existing evaluator already has exact-path and line-overlap predicates (`benchmarks/aft-search/metrics.py:62-87`); Ripwire's method supplies the repository split and multi-file discipline.

## Borrow / do not borrow

### Borrow

| Item | Why | Where it would land in AFT | Cost |
|---|---|---|---|
| Self-contained per-result risk suffix | Lets an agent choose a safe/reused/tested result without three follow-up calls; reuse already-resident facts only. | `crates/aft/src/commands/semantic_search.rs`, `crates/aft/src/commands/callgraph_store_adapter.rs` | Medium: shared freshness/absence contract and compact formatter. |
| Explicit ranking confidence and flat-head warning | A deterministic margin is not probability, but an honest “starting point” marker is better than silent top-k certainty. | `crates/aft/src/commands/semantic_search.rs` response model + bridge formatter | Medium: define margin over exact/RRF order and calibrate it; do not copy the 20% threshold blindly. |
| One truncation envelope across navigation | `shown/total/dropped/reason/lower_bound` makes compact replies composable and auditable. | callgraph adapters, `commands/outline.rs`, bridge renderers | Medium. Existing `hub_summary` supplies the seed shape. |
| Moment-based knowhow router | “When” beats another flag catalog; especially useful before signature changes, risky edits, and after context compaction. | curated knowhow entry plus existing plugin tool descriptions | Small. |
| Transcript-backed discoverability audit | Separates “tool could not answer” from “agent did not reach for it,” with command/result provenance. | `docs/` investigation template or a knowhow recipe | Small. |
| Repository-disjoint LocBench adapter | Tests task-to-file localization, multi-file strictness, and cost on exactly the failure mode Q1 samples too weakly. | `benchmarks/aft-search/` | Large: corpus checkout, frozen adapter, paired runner, clustered analysis. |
| Metal/CUDA/TOML extraction lanes | Closes real unsupported extensions and preserves shader host/device relationships; TOML improves config navigation. | `crates/aft/src/parser.rs`, Cargo grammar deps, callgraph language labels | Medium per lane, with fixtures and resolver policy. |

### Hard do-not-borrow

| Item | Reason |
|---|---|
| Replace AFT hybrid search with lexical-only BM25 | Q1 paraphrases show vocabulary sensitivity, and Ripwire itself keeps several expansion ideas experimental. Retain semantic retrieval; consider BM25 as an additional deterministic lane, not a replacement. |
| `amp = direct callers + co-change file degree` under a graph-impact label | It mixes symbol and file granularity and does not count transitive affected nodes. Keep AFT's `total_affected`, direct callers, and future history risk separate. |
| Rebuild the whole graph in every AFT query to remove the daemon | The Rails cold measurement shows why AFT needs standing/persistent views. Borrow content reuse, not the process model. |
| Auto-write a large skill fleet into every host | The release installer currently activates only Claude/Codex despite broader README wording, and overlapping skills create routing burden. Extend AFT's centralized setup and one router. |
| Ripwire's coarse default edge provenance | AFT's explicit `type_match` versus approximate `name_match` is more actionable; optional compiler provenance can be additive later. |
| Minified XML as AFT's common response format | The useful idea is the budget/truncation contract. AFT's server-rendered text plus structured JSON is easier to evolve and already host-integrated. |
| Treat the current 4.3K claim or 20% cliff as universal constants | Token density, query shape, corpus, and agent tokenizer vary. Measure per surface and disclose the estimator. |
