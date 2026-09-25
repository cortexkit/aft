# Callgraph "corpus drift" forced rebuilds (2026-09-25)

Source: daemon log `~/.local/share/cortexkit/aft/logs/aft-74286.log` (pid 74286, 0.58
pre-release). Code references are to `crates/aft/src` at base `b05d229d`.

## TL;DR

- The line `callgraph cold-build decision: reason=corpus drift; action=force rebuild`
  (`context.rs` `callgraph_store_for_ops`) only means "this root's context holds a pending
  force token". It does not say which of the four minting sites set the token. The label
  "corpus drift" is misleading: in three of the four events nothing drifted except a
  `package.json` mtime or the harness name.
- All four events ran in **configure maintenance** (`ConfigureMaintenanceStage::Callgraph`,
  `configure.rs` ~6082). The job had `warm_callgraph_store = true` and found a pending token
  on the **owner** root's own `AppContext`.
- Two triggers account for the four events:
  1. **Workspace-manifest mtime** (`configure.rs` ~3560). `configure_callgraph_build_key`
     includes `workspace_manifest_fingerprint`, which is path + length + **mtime** of
     `package.json` and `packages/*/package.json`. Any touch of those files (a version bump,
     a merge, a checkout) makes the next full configure non-equivalent. That forces a full
     cold rebuild of the whole repo.
  2. **Stale tokens from watcher gaps and overflows** (`context.rs` ~6512 idle-eviction gap,
     `runtime_drain.rs` ~2083 overflow). The token is minted, then sits pending for hours
     while the watcher keeps the store incrementally fresh. It fires only when a configure
     happens to be non-equivalent. A different harness is enough for that:
     `route_harness` is in `SemanticBackendConfig`'s `Debug`, and so it is in the warm key.
- The evidence checkout did **not** cause the 14:30:53 rebuild. Its session tag on that
  line is a logging artifact: configure-maintenance units run without
  `log_ctx::with_session(job.session_id)`, so they inherit whatever session the executing
  thread last set. The real trigger was `packages/aft-cli/package.json` being rewritten at
  14:14:47Z.
- Borrowers cannot mint or consume the owner's token. The token is a pair of atomics per
  `AppContext`, and every `AppContext` has one canonical root. There was one real
  borrower-side bug, now fixed in this change: `configure.rs` ~3560 minted a token on
  **read-only** contexts too. A read-only context can never fulfil a token, and with one
  pending `callgraph_store_for_ops` returns `Unavailable` forever (`context.rs` ~5563).

## 1. The four events

| Time (Z) | Root (from the following `build_started`) | Token minted by | Why the warm ran |
|---|---|---|---|
| 12:16:59 | magic-context (configure 12:16:57, route 683, runner) | `runtime_drain.rs` ~2083, watcher overflow at **11:25:59** (`callgraph store scheduled for background rebuild after watcher overflow`), **and** `configure.rs` ~3560: `packages/{cli,e2e-tests,pi-plugin}/package.json` mtime 12:00:19 changed the build key. | Manifest change made the warm key non-equivalent. The build was then `deferred by cold build limit (2)`, so the token stayed pending. |
| 12:25:36 | magic-context, `b-74286-932` (configure 12:25:33, route 715, **opencode**) | The same token still pending from 12:16:59 | The harness flipped from runner to opencode, so the warm key was non-equivalent (`route_harness`). The rebuild took 371 s for 2,258 files. |
| 13:15:59 | broca, `b-74286-1243` (configure 13:15:57, route 837, **runner**) | `context.rs` ~6512 `invalidate_artifacts_after_watcher_gap`: broca was **idle-evicted at 10:35:37** (`evicted idle root .../broca`). | The 11:53:50 opencode rebind had the same harness as the 10:04:24 bind, so its warm key was equivalent and it did not warm; the token stayed pending. Watcher refreshes (12:47:29, 13:15:23 `tier2 dead_code: refreshed callgraph store ... changed=16`) kept the store incrementally current. The runner rebind at 13:15:57 flipped the harness and ran the warm, which spent the **2 h 40 min old** token. The rebuild took 39 s for 450 files. |
| 14:30:53 | main aft `/Users/.../CortexKit/aft`, `b-74286-1695`, key `90ff783f3f4c5cf2` (configure 14:30:49, route 1028, runner, session `...merge-r3-...-a1`) | `configure.rs` ~3560: `packages/aft-cli/package.json` mtime **14:14:47Z**. The previous full configure of this root was at 10:22:29 with the same runner harness, so the manifest is the only input that changed. | Build key non-equivalent. The rebuild took 1,181 s for 2,777 files and 424 k references; this is the ~2.9 GiB/h write episode. |

Checks behind the table:
- There were no watcher overflows or ignore-rule changes on broca or the main aft root all
  day. `watcher overflow` hit only magic-context (11:25:59), callosum (14:53:52) and
  worktree roots. Worktrees are not callgraph writers, so their overflows cannot mint
  (`runtime_drain.rs` ~2079 and ~2225 check `callgraph_writer()`).
- Current manifest mtimes: aft `packages/aft-cli/package.json` 14:14:47Z (the other packages
  09-23); magic-context `packages/{cli,e2e-tests,pi-plugin}` 12:00:19Z; broca has only
  a root `package.json` (2026-07-10), so the manifest trigger cannot explain broca.
- The 14:30:46–14:30:49 evidence-checkout configures (7 binds in 3 s) each ended with
  `quiesced unbound root .../evidence-ct-... (cancelled 1 configure maintenance job(s))`.
  Their maintenance never reached the Callgraph stage. The evidence context is also not a
  writer (`callgraph_writer_capability = ... && !is_worktree_bridge && !artifact_owner_read_only`,
  `configure.rs` ~3310), so it could not have started a build.
- The session tag on the 14:30:53 line (`consult-...-r3`) and on 12:25:36 (`ses_f27b1b58...`,
  which did not configure magic-context) does not match the configuring session. This is
  consistent with `drain_deferred_configure_maintenance*` (`configure.rs` ~5549, ~5600)
  never scoping `log_ctx` to `job.session_id`. The `build_started root=` line that follows
  is authoritative.

## 2. Can a read-only borrower force or consume the owner's rebuild?

No. The force token is `callgraph_store_force_requested` and `..._fulfilled` on one
`AppContext`, and each root actor owns its own context. The borrower's `canonical_cache_root`
is its own checkout path. Sharing the owner's index family only means the borrower reads
the owner's published artifacts. `runtime_drain.rs` ~2079, ~2225 and `context.rs` ~6511
already refuse to mint on non-writers.

**Bug fixed here.** `configure.rs` ~3560 minted without a writer check. A read-only root
reconfigured on the same path with a non-equivalent build key got a token it could never
fulfil. Examples: a linked worktree whose `package.json` changed, or a writer/read-only flip.
`callgraph_store_for_ops` then skips `open_readonly` and returns `Unavailable`
(`context.rs` ~5563) until the actor is recreated. The fix adds `&& ctx.callgraph_writer()`,
which matches the invariant already documented at `context.rs` ~6508.
- New test `read_only_worktree_reconfigure_never_mints_unfulfillable_callgraph_force_token`
  fails without the guard and passes with it.
- New test `writer_reconfigure_after_manifest_change_mints_callgraph_force_token` pins
  today's writer behaviour. It passes both with and without the guard, and it is the test
  to flip when the manifest trigger below is fixed.

Related gap, not fixed here: a context that holds a pending token and then *becomes*
read-only (owner lease lost) is stuck the same way.

## 3. Is a full forced rebuild the right response?

Reference cost (docs/investigations, oh-my-pi refresh benchmark): an incremental refresh of
1,400 files writes about 1.9 GiB; a cold rebuild writes about 26 GiB.

| Trigger | Site | Full rebuild justified? | Proposal |
|---|---|---|---|
| Workspace manifest length/mtime changed | `configure.rs` ~3560 via `workspace_manifest_fingerprint` | **No.** A version bump changes no source file. Manifests only affect workspace-package resolution (`clear_workspace_package_cache_under` already runs on every reconfigure). | (a) Fingerprint only the fields that affect resolution (`name`, `main`, `module`, `types`, `exports`, `workspaces`), parsed and hashed, instead of `len:mtime`. (b) When those fields do change, refresh only the files whose unresolved or cross-package references point into the changed packages, or at most the files under the changed package directories, through `refresh_files`. Never a cold rebuild. |
| Watcher overflow / rescan (lost events) | `runtime_drain.rs` ~2083, ~2232 | **No.** Events were lost, but the store already records per-file freshness (`content_hash`, `size`; see `staged_content_matches`, `insert_backend_state_prepared`). | A bounded reconcile: walk the corpus, compare stat, then hash on stat mismatch, against stored freshness. That gives the changed, deleted and new sets, which go to `refresh_files` in the maintenance lane. A cold rebuild is only warranted when the changed set exceeds a large fraction of the corpus. |
| Watcher gap after idle eviction / dead watcher | `context.rs` ~6512 | **No.** Same situation as lost events, over a longer window. | The same reconcile on rebind. Today the token is also **spent lazily and arbitrarily late**: broca's 10:35 token fired at 13:15, after two hours of incremental refreshes had already applied the missed edits. |
| Callgraph config really changed (storage dir, enabled flag, worktree/readonly topology) | `configure.rs` ~3560 | Yes for storage-dir or topology changes (a different store). | Keep, but split manifests out of `configure_callgraph_build_key`. |

Cheap hardening that applies whatever the trigger:
1. **Log the minting reason.** Store a reason alongside the token (for example
   `manifest_changed`, `watcher_overflow`, `watcher_gap`, `config_changed`) and print it in the
   cold-build decision line instead of the fixed text "corpus drift". This investigation had
   to reconstruct every cause from surrounding lines.
2. **Scope maintenance logging to the job.** Wrap `run_configure_maintenance_unit` in
   `log_ctx::with_session(job.session_id)` so these lines stop naming unrelated sessions.
   That misattribution is why the 14:30:53 event looked borrower-driven.
3. (Optional) Do not let an unrelated harness flip decide *when* a stale token fires. Either
   consume gap and overflow tokens promptly through the reconcile above, or drop
   `route_harness`/`route_project_root` from the warm key: they are `#[serde(skip)]`
   routing metadata, and only `Debug` puts them into the key.
