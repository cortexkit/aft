# Storage retention: read-only investigation and cleanup

## Safety and reproduction

The production store was **not cleaned or migrated**. The census uses filesystem
metadata for WAL/SHM sizes, reads JSON manifests and markers, and delegates every
SQLite query to a separate `sqlite3 -readonly` process. No AFT executable or test
was pointed at the production store. No database file set was copied.

Run the advisory dry-run harness from any checkout:

```sh
python3 scripts/storage-retention-report.py --storage-root "$HOME/.local/share/cortexkit/aft"
```

It prints JSON containing a byte census, directory counts, blob row/payload
accounting, explicit gaps and an itemized `would_delete` list with bytes. It has
**no apply mode**. Plans are observations, not authorization: writers/readers can
change after the snapshot and the destructive sweep must recheck under its locks.

The Mac's SQLite 3.54.0 refuses to query a checkpointed WAL-mode database opened
read-only when its sidecars are absent (`unable to open database file (14)`). For
that case alone the harness uses a `file:...?immutable=1` URI, still with
`-readonly`, to prohibit sidecar creation. If a nonempty WAL exists, immutable is
never used and a failed query is a named gap. Python was 3.9.6.

## Identity: do not confuse a checkout with a repository family

* `path_identity::project_scope_key` is the first 16 hexadecimal characters of
  SHA-256 over the canonical checkout path; a gone path falls back to lexical
  normalization. `views/<scope>` and `inspect/<scope>` are private to a checkout.
* `search_index::artifact_cache_key[_with_memo]` hashes the canonical sorted set
  of Git root commits (the path for a non-Git project). `cache-keys.json` maps
  recorded checkout paths to these keys. `callgraph/<artifact-key>`, `index/`,
  `semantic/`, `symbols/` and v1 `blobs/<family>` share that identity. A deleted
  worktree does **not** make a shared family dead if another checkout still exists.
* `artifact_owner::resolve_manifest_dir` stores `artifact-owners/<artifact-key>/owner.json`;
  its `checkout_path` and `project_scope_key` recover both identity classes.
  Linked worktrees borrow the family's artifacts instead of owning a new family.
* `scoped_key` domain-separates standing subtrees with `scoped-v1`; those keys
  are not independently derivable from path hashes. Owner and binding records
  are required rather than guessing their Git identity.
* Legacy `<harness>/callgraph` and `<harness>/inspect` hold flat key-prefixed file
  sets. `Harness::storage_segment` covers `opencode`, `pi`, `runner`, hashed
  `mcp--<client>--...` and `fed--...` namespaces; their cache keys have the same
  meanings as the corresponding root-keyed stores.
* V2 views live at `views/v2/<scope>`; `blobs/v2/<family>/members.sqlite` records
  root bytes, scope, view directory and `last_bind_ms`. It is not v1's memo.

The brief's warning about `idle.root_ttl_minutes` and `pointer.sqlite` was checked
against this base: both exist in source (`config.rs::IdleConfig`,
`views/mod.rs::POINTER_DATABASE`) and the pointer exists in the live store. No
compatibility shim was needed.

## Why the existing cleanup leaves storage behind

| Class | Existing implementation and failure mode |
| --- | --- |
| Callgraph generations | `callgraph_store::gc_old_generations` always kept the newest previous generation, even with no reader. It also let a six-hour absolute TTL override an actually live reader. It ran on cold publication, not on an idle store. |
| View generations | `views/generation.rs::sweep_generations` ran from `commands/configure.rs::run_configure_view_sweep`, not periodically. Pins of living owners were incorrectly expired by renewal age. The file identity guard logged an open-file hazard without preventing unlink. Derived ownership references intentionally require a second sweep after obsolete refs disappear. |
| Dead checkout caches | `run_configure_storage_sweeps` scheduled index, inspect and owner sweeps, not all cache domains. `callgraph_root_sweep` ran only after a callgraph cold build. Index/callgraph liveness was a set of memo keys and keys derived in this process, not root path existence plus last-bind age. A same-key memo write returned early before pruning old roots (`search_index.rs::record_artifact_cache_key_memo`). Inspect used payload age and process-local bound keys, not durable root bindings. Semantic, symbols and v1 views had no storage-wide dead-root sweep. |
| Iterator bounds | The index/callgraph/inspect root sweeps collected and sorted the entire directory before applying a processing limit. That limited deletions, not enumeration or memory. |
| Owner manifests | `artifact_owner::reap_owner_manifests_pass` only removed the small ownership JSON (24-hour stale heartbeat plus missing checkout), not the large caches it identified. Removing identity first makes subsequent cache attribution harder. |
| Idle root eviction | `subc/mod.rs::root_idle_ttl` and `context.rs::[try_]evict_idle_artifacts` drop resident handles and memory caches. They do not delete durable cache directories. The reaper only knows actor roots opened in this daemon lifetime. |
| V1 blobs | `gc/mod.rs::sweep_plane` stopped deleting when under a 2 GiB **per-plane** payload budget. Unreachable content was retained indefinitely below that budget, freed pages were not returned to the filesystem, and the caller's reference mark covered only one checkout despite a shared family. |
| V2 blobs | `gc/family.rs` has the right epoch/touch race protocol and bounded marking, but disk-limit enforcement is scheduled by active-family runtime/admission. Two missing-root sweeps without an elapsed last-bind grace could also evict a temporarily absent volume. |

## Read-only measurements

The first census at 2026-10-05T18:19Z counted 145,953,345,536 allocated bytes
(135.93 GiB). `du -sk` just before it counted 142,739,248 KiB (136.13 GiB); the
store was actively changing. Allocated blocks are not exclusive APFS extents and
cannot predict exact post-cleanup free disk space.

The finalized census, dry-run totals and verification evidence are recorded below
after the implementation and its safety tests.
