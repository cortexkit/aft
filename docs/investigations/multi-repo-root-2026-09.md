# A session root that holds many repositories (2026-09)

Evidence: daemon log `~/.local/share/cortexkit/aft/logs/aft-74286.log` (pid 74286,
2026-09-25T10:04Z to 2026-09-26T03:27Z) and the artifacts under
`~/.local/share/cortexkit/aft/{index,inspect,semantic}/3bd525e8f129f7b6`, read only.
The root is `~/Work/Projects/CortexKit`. It has no `.git` of its own; 36 of its 41
subdirectories have one, and 40 top-level `target/` or `node_modules/` directories sit
inside those child repositories. The root was bound from 17:01Z to 03:27Z (about 10.4 h).

## What AFT does today

**Choosing the root.** Nothing detects this layout. The bridge pool keys a bridge by
`canonicalizeProjectRoot(dir)` of the session directory (`packages/aft-bridge/src/pool.ts`,
`project-identity.ts`). `handle_configure` canonicalizes that path
(`crates/aft/src/commands/configure.rs`, `canonical_cache_root`) and does not walk up or
down to a git top level. The only guard that looks at the root itself is the home-folder
check: `degraded_reasons: ["home_root"]` when the root equals `$HOME`. The worktree probe
(`detect_worktree_bridge`) runs `git rev-parse` in the root, which fails here, so the root
is treated as a plain, writable, non-worktree root. `current_git_head(root)` is `None`, so
no HEAD check backs trigram cache reuse.

**Which indexes build.** Every index the config enables, over the whole tree:
- trigram: `walk_project_files` uses the `ignore` walker. It reads each child's
  `.gitignore`, so child `target/` and `node_modules/` directories are not indexed.
  26,340 files at bind and 29,586 later. `cache.bin` is 273,540,707 bytes.
- semantic: 14,082 files, 272,617 chunks (`semantic collect` at 17:01:29Z). The
  artifact is 1.3 GB.
- Tier-2 inspect: 21,215 files (23,713 later). The database is 360,943,616 bytes plus a
  208,723,352-byte WAL.
- callgraph: nothing is stored for this key (the `callgraph/3bd525e8f129f7b6` directory
  is empty).

**Watcher events.** The macOS backend watches the root recursively
(`watcher_backend/mod.rs`, `RecursiveMode::Recursive`). Each event from every child
repository reaches this root's watcher. That includes events in ignored build output
(`cargo` writing `target/`, package managers writing `node_modules/`), because ignore
rules are applied after delivery, not at the FSEvents subscription. When FSEvents drops
events (`reason=user_dropped`), `refresh_project_after_watcher_rescan` calls
`refresh_project_corpus(ctx, "watcher overflow", false)`
(`crates/aft/src/runtime_drain.rs`). That call starts a full trigram rebuild
(`spawn_search_corpus_refresh_admitted`), a semantic corpus refresh, and marks Tier-2
stale. A change to any child `.gitignore` goes through the ignore-rule path, which
triggers the same refresh. The rebuild path also appears to write `cache.bin` twice:
`build_with_limit_to_cache_dir` streams it once, then `write_to_disk` rewrites base plus
an empty delta. This should be confirmed before relying on it.

## Costs seen in the log (root `…/CortexKit`, 17:01Z–03:27Z)

| Item | Count / size |
| --- | --- |
| watcher overflows (`watcher_rescan … reason=user_dropped`) on this root | 36, carrying 1,044,797 raw events (at most 162,713 in one) |
| "started search index refresh after watcher overflow" (daemon-wide, mostly this root) | 51, plus 2 after an ignore-rule change (`paths=[.gitignore]`) |
| full trigram builds of this root (`build_ready plane=search`) | 35: 17 in the 20:00Z hour, 5 in the 19:00Z hour |
| trigram bytes rewritten | 35 × 273.5 MB ≈ 9.6 GB, or about 19 GB if each rebuild also rewrites through `write_to_disk` |
| Tier-2 runs started | 400 (84 over all ~21k files, 269 small, 47 empty), about 25–50 per hour; 2,452 s of total run time |
| semantic bind catch-up | collect at 17:01:29Z, embedding finished at 19:39:00Z (about 2 h 37 min): 249,588 chunks in 3,900 batches. It persisted a 1,300,782 KB delta, then compaction rewrote 1,351,574,839 bytes |
| later semantic refreshes | a "watcher batch" every few minutes, each taking a cold-build slot (`queued behind concurrency cap (2)`) |

The high write rate and the 74% unexplained share match this picture. The two largest
writers here (trigram rebuilds and Tier-2 commits) were not credited as physical bytes
before this change. The change alongside this report credits them.

## Options

### A. Detect the layout and run in a reduced mode (trigram only)
Treat a root that has no `.git` and at least N children with `.git` (N=2 or 3) like
`home_root`: add a `degraded_reasons` entry such as `multi_repo_root` and force
`indexes.semantic = false`, `indexes.callgraph = false`, and Tier-2 off, but keep
trigram.
- *Effect here:* removes the 1.3 GB semantic artifact, the 2.6 h catch-up, and the
  400 Tier-2 runs with their 570 MB database. Trigram overflow rebuilds (about 9.6–19 GB)
  continue, because the recursive watcher still sees every child's `target/`.
- *Agent loses:* semantic search, inspect/code-health, and callgraph across the folder.
  grep, glob, read, and edit keep working.
- *Risk:* low. It reuses the existing `home_root` mechanism (`configure.rs`
  `search_disabled_for_home` and related flags, `feature_status`, the
  `humanize_degraded_reasons` text). The detection costs one `read_dir` of the root. A
  monorepo whose root does have `.git` is not affected.

### B. A reduced mode that also stops full trigram rebuilds on overflow
Option A, plus: in this mode an overflow invalidates only the child repository the
dropped events came from, or trigram is dropped too and grep/glob use the bounded
fallback walk (the path `build_denied` already uses).
- *Effect here:* with trigram off, the steady writes are close to zero.
- *Agent loses:* indexed grep speed across about 30k files. The fallback walk is bounded
  and slower.
- *Risk:* medium if trigram stays on with per-child invalidation (new code in the
  watcher/rescan path). Low if trigram is simply turned off.

### C. Refuse to index, with a clear status (like the old home guard)
Have configure return degraded with every index off, or have the bridge refuse to spawn.
- *Effect here:* no index writes at all.
- *Agent loses:* all indexed features. The bridge-level refusal (`HomeProjectRootError`)
  was removed for `$HOME` because dotfile and migration work needs a working bridge.
  Refusing here would break editing across repositories from this folder, a normal
  workflow.
- *Risk:* low to implement, but user-hostile. It is better expressed as option A or B
  with every index off than as a refusal.

### D. Bind each child repository separately
Keep the session at the folder, but route indexed operations to per-child roots, each
with its own `.git`, HEAD check, and artifact key (keys already exist for child roots
opened directly: this log loads `…/CortexKit/broca`, `…/magic-context`, and others as
their own roots).
- *Effect here:* artifacts are shared with sessions opened inside each repository, so
  nothing is duplicated. Watchers are per repository, so one repository's build churn
  does not force a rebuild of the other 35. HEAD-validated cache reuse works.
- *Agent loses:* nothing in principle. A query across the whole folder becomes a
  fan-out and merge over child indexes.
- *Risk:* high. Root identity is one value per bridge today (`canonical_cache_root`,
  one watcher, one set of stores). Fanning out search, semantic, and inspect results is a
  large change. The memory cost of 36 resident roots needs the existing idle eviction to
  keep up.

## Can the home guard be extended?

Yes, for options A to C. The guard lives in one place, `handle_configure`
(`resolve_home_dir() == canonical_cache_root` sets `home_match`). It already flips the
index flags off, feeds the configure warm key and fingerprint, skips artifact-key
resolution (`artifact_key_needed = !home_match && …`), and reaches the status and
sidebar text through `degraded_reasons`. Generalizing `home_match` into a
"container root" predicate (home, or no `.git` with several `.git` children) with its own
reason string is a local change. It should keep trigram configurable separately so
option A stays possible. The Pi plugin's eager-configure skip (`isHomeDirectoryRoot` in
`packages/pi-plugin/src/index.ts`) and the bridge's `isHomeDirectoryRoot`
(`packages/aft-bridge/src/pool.ts`) only compare against `$HOME`. They could take the
same predicate so a container folder does not warm a bridge eagerly. That is optional
once the Rust side degrades cheaply. Tier-2 has no home-root gate today: nothing under `crates/aft/src/inspect` checks
`is_home_root()` or `degraded_reasons`. A container-root mode therefore needs an
explicit Tier-2 off switch.

**Recommendation:** option A now (small, reuses the guard, removes the semantic and
Tier-2 load). Add B's "trigram off in container mode" as a config default the user can
override. Treat D as the long-term design.
