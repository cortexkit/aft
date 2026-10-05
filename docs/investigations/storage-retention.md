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

### Final snapshot: 2026-10-05T22:03:45Z

Full, itemized stdout is committed in
[`storage-retention-live.json`](storage-retention-live.json): **3,684 generation
candidates**, with directory, exact generation name, reason and allocated bytes;
144 blob database probes, 114,613 payload rows, and **zero probe gaps**. The
following table accounts for all **142,676,443,136 bytes (132.877 GiB)** in that
snapshot. Sizes are allocated bytes; coordination files are counted with current
data. `dead/unknown` includes recognised missing checkouts and unattributed keys,
not a claim that they are all safe to delete immediately.

| Store | Live/current + coordination | Live/superseded, unheld | Dead/unknown root | Shared/operational |
| --- | ---: | ---: | ---: | ---: |
| views (v1) | 19,383,881,728 | 2,553,126,912 | 22,543,986,688 | 0 |
| views/v2 | 53,342,208 | 2,186,997,760 | 622,354,432 | 0 |
| callgraph | 12,722,016,256 | 7,184,523,264 | 77,803,520 | 0 |
| inspect | 2,868,174,848 | 0 | 1,272,623,104 | 0 |
| index | 4,572,950,528 | 0 | 174,067,712 | 0 |
| semantic | 7,246,589,952 | 0 | 432,852,992 | 182,235,136 |
| symbols | 710,643,712 | 0 | 2,648,334,336 | 0 |
| opencode/callgraph | 2,193,518,592 | 981,467,136 | 376,250,368 | 40,960 |
| opencode/inspect | 386,248,704 | 0 | 4,498,391,040 | 0 |
| pi/callgraph | 398,258,176 | 151,863,296 | 1,187,840 | 0 |
| pi/inspect | 98,131,968 | 0 | 253,952 | 0 |
| mcp--claude-code--…/callgraph | 2,818,723,840 | 0 | 0 | 0 |
| mcp--claude-code--…/inspect | 324,149,248 | 0 | 0 | 0 |
| runner/callgraph | 117,497,856 | 0 | 0 | 0 |
| runner/inspect | 17,928,192 | 0 | 0 | 0 |
| blobs (both layouts) | 0 | 0 | 0 | 36,801,339,392 |
| All other storage, excluding the operational cells above | 0 | 0 | 0 | 6,074,687,488 |
| **Total** | **53,912,055,808** | **13,057,978,368** | **32,648,105,984** | **43,058,302,976** |

**Answer:** 12.161 GiB of unheld superseded generation files, plus 30.406 GiB
associated with missing/unrecognised roots, is cache dead weight by the requested
classification. Together that is **42.567 GiB**, before shared blob garbage.
It is not all immediately reclaimable: missing-root binding/mount proof and
process protection still apply. In this snapshot no recognised missing-root
directory met all the conservative legacy-history/grace checks; those bytes are
not smuggled into the immediate generation plan.

### Shared content

| Blob measure | Bytes | Meaning |
| --- | ---: | --- |
| Physical allocated file-set bytes | 36,801,339,392 | Includes registry/alias databases, sidecars, segments and overhead. |
| Payload lengths in 144 plane databases | 31,627,620,671 | SQLite's `length(payload)`, not an estimate from file size. |
| Payload named by any live current/reader-held manifest | 14,664,524,437 | Content keys are marked from every live retained manifest; current `.ref` owners protect only the derived SQLite file, not an obsolete manifest's semantic keys. |
| Payload not named by those manifests | 16,963,096,234 | **15.798 GiB** of candidate content; assembly pins and the v1 15-minute age floor can protect additional keys. |
| Already-free SQLite pages | 4,745,641,984 | **4.420 GiB** still inside files; deleting rows alone does not return these bytes. |

Do not add payload lengths to physical allocated bytes as though they were
disjoint extents. SQLite page overhead and APFS sharing make that arithmetic
wrong. The blob garbage is material, but an exact post-cleanup `df` saving is not
deducible from a read-only logical census.

### Dry-run deletion output

| Candidate class | Entries | Allocated bytes |
| --- | ---: | ---: |
| Root-keyed callgraph generations | 32 | 7,184,523,264 |
| Legacy opencode callgraph generations | 15 | 981,467,136 |
| Legacy pi callgraph generations | 2 | 151,863,296 |
| V1 and V2 view generation resources | 3,635 | 4,740,124,672 |
| **Generation plan total** | **3,684** | **13,057,978,368** |

The cited `c5fd16d858c5f6c5.g1790302570493041000.38977.sqlite` file set is
**2,163,580,928 allocated bytes** including sidecars, rather than the placeholder
main-file estimate of 1.3 GB. Its pointer names a different generation and its
reader protections were not live at the snapshot. The full JSON contains the
exact file-set names, including this candidate.

These are advisory deletion candidates, not a shell `rm` recipe. SQLite handles
and writer/publication locks are deliberately not acquired by the live dry run;
the daemon must recheck them when applying cleanup. Failed read-only probes would
have been explicit gaps and protection, never deletion authority. The differing
18:19 and 22:03 totals reflect the operator's active daemons/other work, not an
apply operation by this investigation.

## Implemented retention policy

* **Seven days** without a bind plus a definitely absent checkout is required.
  Seven days matches existing memo-eviction grace, tolerates short disconnects,
  and is ample for disposable worktrees that will not return. A new binding is
  written on every configure, including same-key rebinds, before cache acquisition.
* Bind records retain the checkout's filesystem anchor/device (volume serial on
  Windows). An absent/changed anchor retains the cache. Old records without mount
  identity are eligible only for disposable `cortexkit/alfonso/worktrees` paths;
  other unattributed legacy volumes require renewed binding evidence or operator
  review. A path-only compatibility guess would be unsafe.
* Unknown keys receive their own durable first-observation grace and a payload
  inactivity check. Living legacy owners lacking the new residency marker defer
  their deletion. Running actors publish PID/process-instance reader markers so
  another daemon can see residency; a stopped living process is not timed out.
* `storage_retention::schedule` runs from configure and the health worker, once
  per storage root per **10 minutes**, independent of cold rebuilds. A persistent
  iterator takes **64 entries** before filtering; a five-second pass budget and
  4,096-entry recursive deletion/protection bounds refuse incomplete snapshots.
  Binding history is capped at 8,192 entries and obsolete, cache-less binding
  records are pruned in 32-entry batches. This covers historical roots, not only
  the current actor map. Legacy harness partitions are removed by key, never by
  deleting a whole harness directory.
* View ownership enumeration is capped at 65,536 entries, with overflow refusing
  deletion. Temporary manifests are gathered once, rather than rescanning the
  whole directory for every old generation. Current pointers, living assembly
  pins and reader markers win; a `.ref` retains its owner's SQLite file only.
* Callgraph cleanup no longer preserves an unheld previous generation or overrides
  a living reader by age. A publication lock covers final rename through pointer
  swap and collection. The background pass can reuse its own verified writer
  capability, but never a foreign writer's lease. SQLite file-set mutation checks
  are enforcement, not logging-only hazard reports.
* V1 family GC marks all remaining checkout manifests and assembly protections
  under a pin-admission barrier, then deletes old unreferenced rows even below
  budget. SQL takes at most 4,096 rows with a resumable keyset cursor. V2 retains
  its epoch/touch protocol, gains bounded membership reads and the same last-bind
  and mount checks before consecutive-sweep removal.
* Freed pages are compacted through SQLite's own connection/VFS, outside the
  bind/pin admission barrier. The SQLite progress callback stops on cancellation
  or a 60-second deadline; busy/unfinished compaction records a retry marker and
  `blob_reclaim_deferred`. No raw database/WAL/SHM copying is used.
* Health/status publishes examined/removed root and blob counts, generation and
  byte counts, retained reasons, pruned history, errors, cancellation and early
  stop/deferred-compaction flags. Context generation/lifecycle signals prevent
  a superseded or set-aside root's maintenance from continuing unchecked.

The old age-only index/inspect/callgraph helpers remain for isolated lock-policy
regression fixtures, but production scheduling no longer calls them. Disk-pressure
LRU eviction remains a separate policy; this change is abandonment/generation
retention, not a new disk-budget configuration.

## Verification and mutation evidence

All local cargo commands used a throwaway `HOME` and all four XDG homes, with real
`CARGO_HOME`/`RUSTUP_HOME` preserved and `AFT_STORAGE_DIR` unset. None used production
storage. Cargo was **1.99.0**, rustfmt **1.10.0-stable**, Biome **2.4.7**, Python
**3.9.6**, SQLite CLI **3.54.0**.

Before the fixes, `cargo test -p agent-file-tools --lib -- storage_retention`
failed all five initial oracles: unheld previous generation, living reader age,
living assembly pin age, open derived file set, and orphan blobs below budget.
The shared-derived-owner oracle was added after inspecting `.ref` retention and
also failed before its fix (`removed=0`, expected `1`).

| Gate | Result |
| --- | --- |
| `cargo test -p agent-file-tools --lib -- storage_retention` | **20 passed** after restoration of every mutant. |
| `cargo test -p agent-file-tools --test integration per_checkout_registry` | **16 passed**, one intentionally ignored subprocess entry point. |
| `cargo test -p agent-file-tools --test integration pins_gc_test` | **4 passed**. |
| `cargo test -p agent-file-tools --bin aft` | **121 passed**. |
| `cargo test -p agent-file-tools --test list_envelope` | **172 passed**; internal budget sites registered as exclusions. |
| Existing lib filters `gc_old_generations`, `artifact_owner`, `views::generation`, `views::eviction` | **3, 18, 8, 12 passed**, respectively. |
| `cargo fmt --all -- --check` | Exit 0, silent-on-success gate. |
| `bun run lint` | **695 files checked**, no fixes. |
| `python3 scripts/storage-retention-report.py --self-test` | **3 passed**, including a real CLI file-set/mutation-time check. |
| Read-only production harness | **144 SQLite plane probes / 114,613 rows**, zero gaps, full output attached. |
| Search-quality gate | Exact recall **16/16**, concept **26 rows** passed. Full paged real-query evaluation hit the outer 30-minute timeout; the unchanged-score comparison was not reached. |
| Windows cross-check | Skipped as requested; operator runs it. |

`lib.rs` registration and the cache-helper annotation in `search_index.rs` touch
ranking-fenced files, but no query/routing/scoring code changed. The branch's
`engine_unwired`, `targeted_mechanism: none` descriptor is committed. The full
quality gate initially needed corpus provisioning, then reached the real-query
runner after the exact/concept successes; there is no claim of a full benchmark
pass. Scoped rust-analyzer inspection remained partial while indexing/checking;
the actual cargo test compilations provide the authoritative native typecheck.
The macOS linker emitted its existing oversized `__eh_frame` warning.

### Mutations

For every control, the specific files were staged as the live state first, the
working `git diff --stat` was empty, a marked code mutation produced a nonempty
diff, the named oracle went red, and `git checkout -- <path> && touch <path>`
restored an empty working diff. Each following selected oracle was the **only**
failure in its run; the full restored 20-oracle suite passed. The below-budget
mutation additionally ran all 20 oracles, with the other **19 passing**. The
read-only flag mutation ran all three Python checks, with the mount/binding and
real-file-set checks still passing.

| Neutralised rule | Exact oracle suffix | Failure |
| --- | --- | --- |
| V1 binding grace | `storage_retention_keeps_a_recently_bound_missing_root` | removed 1, expected 0 |
| Missing-path prerequisite | `storage_retention_keeps_a_live_root_even_with_old_binding` | removed 1, expected 0 |
| Volume identity | `storage_retention_keeps_a_disconnected_volume` | removed 1, expected 0 |
| Unknown-key observation grace | `storage_retention_unknown_keys_receive_an_observation_grace` | removed 1, expected 0 |
| Iterator cap | `storage_retention_bounds_enumeration_and_resumes` | examined 80, expected 64 |
| Lifecycle cancellation | `storage_retention_cancellation_leaves_the_root_untouched` | cancellation ignored |
| V2 binding grace | `storage_retention_v2_recent_missing_root_survives_repeated_sweeps` | fresh member removed |
| Previous generation collection | `storage_retention_removes_unheld_previous_generation` | unheld previous file remained |
| Reader protection | `storage_retention_live_reader_has_no_absolute_expiry` | living reader's file deleted |
| Publication serialization | `storage_retention_serializes_with_callgraph_publication` | removed 1 while publication lock held, expected 0 |
| Assembly pin protection | `storage_retention_live_assembly_pin_has_no_absolute_expiry` | removed 1, expected 0 |
| Open SQLite protection | `storage_retention_keeps_open_derived_file_set` | removed 1, expected 0 |
| Shared owner's obsolete metadata | `storage_retention_shared_derived_owner_does_not_retain_obsolete_manifest_or_trigram` | removed 0, expected 1 |
| Living owner despite stale heartbeat | `storage_retention_owner_reap_keeps_a_live_process_without_heartbeats` | reaped 1, expected 0 |
| Below-budget orphan collection | `storage_retention_collects_unreferenced_blobs_below_budget` | deleted 0, expected 1 |
| Sibling manifest marking | `storage_retention_blob_mark_includes_sibling_manifests` | deleted 1, expected 0 |
| Physical page reclamation | `storage_retention_blob_compaction_returns_free_pages` | file failed to shrink |
| CLI read-only open flag | `test_sqlite_cli_is_always_readonly_and_never_ignores_a_wal` | argv lost `-readonly`; other two Python checks passed |

The old tests asserting absolute expiry and unconditional previous-generation
retention intentionally changed contract: they contradicted the requested rule
that living owners must never lose their cache. Their replacement includes the
positive collection-after-release assertion. V2 removal-race fixtures now age
both binding clocks so their previous abandonment/pin-race claims remain real.
