# Umbrella sessions: use child repos' indexes, never index the parent

Status: reviewed with the operator 2026-09-26. Replaces Feature A of the 2026-07 multi-root sessions design (signed off 2026-07-04, never built). Feature B of v3 (read-only cross-root search) is built and is the base this uses.

## Problem

A session opened in a plain folder that holds several git repos (for example `~/Work/Projects/CortexKit`, 36 repos) is indexed today as one project covering the whole tree. Measured over 10.4 hours on that folder: 36 watcher overflows, 35 full rebuilds of a 273 MB search index, 400 code-health runs, and one 2.6-hour embedding pass over about 250,000 chunks. Every build step in any child repo triggers work in the parent. Nothing in AFT recognises this layout; the only guard is the home-folder one, which applies to `$HOME` exactly.

## Decision

When a session's root is not itself inside a git repo and has git repos beneath it, the session becomes an umbrella session:

- The parent builds nothing: no search index, no semantic index, no callgraph, no code-health scans, no watcher for indexing. It behaves like the home-folder mode for its own tree.
- Index-backed tools serve from the child repos' own indexes, opened read-only through the existing borrowed-artifact path (the same one `aft_search path:` uses today). Opening the parent never builds or repairs a child's index.
- `read`, `write`, `edit`, `bash`, outline and zoom work as they do now across the whole tree, including loose files between repos. Containment for permissions stays the umbrella folder.

## Discovery

- Runs at configure only when the root is not inside a git repo. A session inside a repo behaves exactly as today.
- Walks at most 3 levels below the root and stops descending at the first `.git` (a repo nested inside another repo belongs to the outer one). It skips `node_modules`, `target`, `.cache`, ignored and hidden build folders, and does not follow symlinks. The bound is applied inside the walk.
- At most 32 repos. Above that, the session does not become an umbrella: it falls back to the home-folder mode (no indexes; grep and glob walk the files) and says why in status.
- The depth (3) and repo count (32) are fixed, with no config keys.
- `$HOME` is unchanged: the home guard wins and discovery does not run there.
- Re-scanned on configure only; a newly cloned repo appears on the next configure.

## Serving

For each discovered repo, per index kind, the open result is one of:

- **Fresh.** Serve it.
- **Stale** (the index exists, but no session currently keeps it up to date and files have changed since it was built). Serve it and disclose it (below).
- **Absent** (never indexed). Report it as not indexed. `grep` and `glob` fall back to walking that repo's files; `aft_search` reports the repo as not indexed rather than searching it without an index; `aft_callgraph` refuses for paths in that repo.

Rootless queries (`aft_search`, `grep`, `glob` with no path) fan out across repos with an index and merge. Results use paths relative to the umbrella. A path-carrying query goes to the one repo that contains the path. Merging rules follow v3: lexical scores merge directly; semantic scores merge directly only when the repos used the same embedding model, otherwise by per-repo rank.

`aft_inspect` without a scope refuses in an umbrella session and asks for a path, so one call never scans many repos.

## Staleness: who needs to know, and what they get

The agent is the reader who acts on staleness: it decides whether to trust a result or confirm with grep. The user rarely reads tool output and should not have to rely on the agent to relay it. So each gets its own signal:

- **The agent**, in the result itself: a line naming the stale repos and how long since each was last updated, plus a result-level note when the top results come from a stale repo. The wording says what to do ("files changed since then may be missing; confirm with grep"), not just an age.
- **The user**, in the sidebar, the OpenCode 2 footer and Pi's status line: "umbrella: 30 repos, 4 stale, 2 not indexed", with the per-repo list in `/aft-status`. This is where a person sees it without asking.
- **Exact and literal correctness.** For the trigram lane, staleness can produce wrong answers ("no match" for text that now exists). A stale repo therefore also gets a bounded check of files modified after the index was built: those files are searched directly and merged, the check is capped per query and reports what it examined, and a repo whose changed set exceeds the cap is marked as partially covered. Semantic results from a stale repo are served without this correction; for ranking hints, age is acceptable.

## Out of scope

- Keeping child indexes up to date from the umbrella session (a watcher per repo). Opening the child repo in its own session does that.
- Building a child's index on demand from the umbrella.
- Cross-repo refactors.

## Callgraph staleness

`aft_callgraph` on a stale repo answers from the stored graph and says, per query, when the queried file changed after the store was built (an mtime check on that one file), so the agent knows edges may be missing.

## Resolved in review

- Above 32 repos: fall back to home-folder mode (Ufuk).
- Staleness shown to the user in the sidebar, OpenCode 2 footer, Pi status line and `/aft-status`, not only in tool output (Ufuk).
- Limits fixed at 3 levels and 32 repos, no config keys.
