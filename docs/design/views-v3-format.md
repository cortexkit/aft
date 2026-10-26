# Views v3 format: namespace and reader-floor foundation

This document records the v3 namespace audit, the floor protocol, and the v3
registry and plane store contract: schemas, application IDs, PRAGMA checks,
the reservation ledger and lock, open modes, accounting, budgets and floor,
the retention collector and the power-loss replay result. No production
caller publishes v3: v3 writes happen only in tests and in harness-selected
isolated storage.

## Release walker audit

Audit sources are the immutable local release tags (authorized because the
campaign's frozen evidence pack contains no release-source copies):

- **A — v0.60.0:** `988f4f3e52b9b825ad71cc75e41352dc6856efa4`.
- **B — v0.61.0:** `8aceb397b5d9836004b548ea1a5aa8f1d75cd0e8`.

`git fetch origin tag v0.61.0 --force` and `git rev-parse <tag>^{commit}`
verified these identities. References below are repository-relative source
paths and line ranges **at the indicated release**, not current-main lines.
The audit reads `git show <sha>:<path>`; it does not inspect a live store.

The namespace under review is
`<storage>/blobs/v3/<family>/`, including `members.sqlite`, plane stores and
their WAL/SHM files, and member directories at `views/<scope>/` beneath that
family. A v3 member must never appear in a v2 registry. In particular, v3
member directories must **not** be placed at `<storage>/views/v3/`: the old v1
GC visits every direct child of `<storage>/views/` except `v2`.

The verdicts concern ordinary AFT-created layouts: no forged registry paths,
no path-traversal family IDs, and no symlink/alias redirect from an old
namespace into v3. These are namespace-isolation verdicts, not a security
boundary against corrupt or hostile storage metadata. Both releases trust
`view_dir` text in v2 registry rows (A `crates/aft/src/views/registry.rs:1037-1065`;
B `:1047-1075`). A forged row could send marking, measurement and removal into
v3. That is why separate registration is a mandatory invariant, not merely a
naming convention.

### GC, eviction and explicit legacy pruning

| Walker | Release A source | Release B source / difference | Namespace and verdict |
|---|---|---|---|
| `gc::sweep_planes_bounded`, sibling views marking | `crates/aft/src/gc/mod.rs:131-173,245-325` | Same views-root/filter, `:132-173,245-317` | Direct children of `<storage>/views/`, excluding `v2`; reads each child's manifests, pins and readers and may reclaim dead protection. **No v3 descent** with the chosen nested blob root. |
| v1 plane sweep and reclaim | `crates/aft/src/gc/mod.rs:174-224,327-459` | `:175-216,327-436`; bounded incremental reclaim replaces full vacuum/retry-marker creation | Addresses only `<storage>/blobs/<family>/{semantic,callgraph}.sqlite` and the associated old reclaim marker. Does not recursively list `blobs`. **No v3 files** for valid old family IDs. |
| v2 family GC marking, dead protections and missing-root deregistration | `crates/aft/src/gc/family.rs:234-309,430-465,513-557,677-820` | `:234-307,453-484,535-579,697-841`; page-capacity skip and bounded reclaim added | Opens plane stores through the v2 path builder, walks only registered v2 members' view directories. **No v3 descent**, conditional on no v3 row in v2 registry. |
| v2 member measurement, SQLite-open check, eviction and interrupted eviction | `crates/aft/src/views/eviction.rs:301-400,421-474,558-745` | `:303-554,573-641,720-907`; immutable-generation size cache added and payload aggregates removed from family measurement | Family tree is `blobs/v2/<family>`; member trees come from v2 registry `view_dir`. Counts and removes only those trees. **No v3 descent** under the registration invariant. |
| v2 store path/registry registration | `crates/aft/src/blob_store/v2.rs:370-378`; `crates/aft/src/views/registry.rs:199-203,388-434` | Same roots; registry adds a process-lifetime size cache, not a namespace | `blobs/v2/<family>` and `views/v2/<scope>` remain separate from v3. |
| `aft cache prune-legacy` CLI (only native `cache` subcommand) | `crates/aft/src/cli/prune_legacy.rs:45-89`; `crates/aft/src/main.rs:225-231` | Native CLI/helper unchanged byte-for-byte; package forwarding at `packages/aft-cli/src/commands/cache.ts:10-25` | Parses dry-run/`--yes` then delegates to migration pruning; **no independent v3 walk**. |
| Pruner inventory, sizing, rename-aside, re-census and deletion | `crates/aft/src/migration/prune_legacy.rs:45-77,157-187,242-331,694-735,752-886` | File unchanged byte-for-byte between A and B | Only valid-key children under `index`, `semantic`, `callgraph`, `artifact-owners`, plus root children with `.prune-legacy-` prefix. Import eligibility reads `migration/v2/<key>/imports.sqlite`. Neither inventory nor deletion selects `blobs/v3`. **No v3 count/touch/delete**. |

A leftover `.prune-legacy-*` tree is owned by the old pruner; manually moving
v3 data into such a tree would make it prune-eligible. Similarly the public v1
sweep path builder does not itself reject `family = "v3"`, but ordinary callers
supply valid family identities, and retention filters for 16 hex characters.
Neither exception is a recursive route into the chosen v3 family tree.

### Every storage-retention lane

All paths in the next table are in `crates/aft/src/storage_retention.rs`.
The direct-child storage listing used to discover harness domains is not a
recursive walk: only `opencode`, `pi`, `runner`, `mcp--*` and `fed--*` add flat
`callgraph`/`inspect` domains. Seeing the name `blobs` or `v3` in a parent
listing is not descent into it.

| Lane | A lines | B lines | Root/filter and v3 verdict |
|---|---|---|---|
| Root/domain discovery and lexical cursor | `44-51,830-974,1946-2040` | `44-51,835-979,2405-2503` | Only `views`, `callgraph`, `inspect`, `index`, `semantic`, `symbols`, and the harness flat domains above. Requires a 16-hex key and directory, or flat `<key>.current`/`<key>.sqlite`. **No v3 count/touch/delete**. |
| Binding history, ownership, eligibility and unknown-key clocks | `296-319,619-827,1361-1405` | `299-322,622-832,1784-1828` | `cache-keys.json`, `retention/roots`, `artifact-owners`, `retention/unknown`. Metadata only; **no v3 walk**. |
| Due-first | `1728-1836,1971-1984` | `2181-2291,2431-2448` | Due valid keys in the non-flat domains. B raises cap 64→256, prioritizes views and batches retirement admission rechecks; roots remain unchanged. **No v3 walk**. |
| Orphan views / membership lookup | `1407-1449,1607-1662,1855-1929` | `1830-1872,2035-2089,2309-2388` | Direct valid-key children of `views`, membership from existing `blobs/v2/*/members.sqlite`. The key filter rejects `v3`; the actual v3 directory is elsewhere. **No v3 walk**. |
| V1 blob-family collection | `980-999,2087-2103,3780-3817` | `985-1005,2572-2589,5250-5287` | One direct family under `blobs` per cursor; family must be a 16-hex directory. `v2` and `v3` fail before GC. **No v3 descent**. |
| V2 family collection / generation sweep / family GC | `2120-2211` | `2605-2696` | One valid family under `blobs/v2`; opens existing registry, then registered members' generation directories and v2 planes. **No v3 descent** under the separate-registration invariant. |
| Cache retirement / validation / trash reaper | `1099-1355,2081` | `1104-1778,2562-2566` | Selected cache domain trees and their `.retention-trash/retired-*` trees. B additionally retires mount-point contents into mount-local trash and records pending reaps. These are still selected old cache roots, never `blobs/v3`. **No v3 count/touch/delete**. |
| Binding prune | `1950-1951,3819-3972` | `2405-2411,5289-5442` | Visits `retention/roots/*.json`, checks old caches, ownership, residency and v2 registry/views, then removes only a due binding JSON. **No v3 walk**; v3 presence does not independently preserve an old binding record. |
| Barrier, pass/cursor/report and reclaim metadata | `980-999,2087-2103,4054` | `985-1005,2572-2589,5546` | Coordination files beneath `retention`; not blob payload enumeration. **No v3 descent**. |

### Other view and storage entry points

| Walker / entry point | A source | B source / difference | Verdict |
|---|---|---|---|
| Alias store | `crates/aft/src/alias/mod.rs:313-332,973-982` | Same path construction and key validation; only module registration changes | Opens only `blobs/<artifact_key>/oid-alias.sqlite`, with validated key. No directory enumeration. **No v3 touch**. |
| V1 blob open, corruption quarantine and sidecars | `crates/aft/src/blob_store/mod.rs:470-508,708-721,845-868` | `:475-513,722-735,859-882`; reclaim policy and membership-index optimization change, not path routing | Validated artifact key selects `blobs/<key>/<plane>.sqlite`; quarantine renames that file and its WAL/SHM only. **No v3 touch**. |
| `ViewStore` open / existing / permissions | `crates/aft/src/views/mod.rs:747-784` | `:749-786`; module additions only | V1 opens `views/<scope>`; explicit v2 paths come from registration, never v3. Existing opens can tighten directory permissions, but only at the supplied old view directory. **No v3 touch** under separate registration. |
| Generation reference listing, GC, temp removal and deferred derived checkpoint | `crates/aft/src/views/generation.rs:64-89,95-285,289-559` | File unchanged byte-for-byte | Enumerates one `self.view_dir()`, handles named generation files plus WAL/SHM and pins/read markers; does not discover new storage roots. **No v3 count/touch/delete** unless explicitly passed a v3 directory, which old routing never does. |
| Context cache routing and view binding | `crates/aft/src/context.rs:6880-6960,7835-7850,8551-8558` | `:7622-7721,8593-8603,9311-9318`; runtime coordination changes, no v3 route | Fixed `inspect/<scope>`, `callgraph/<artifact_key>` and existing view/registry paths. Production context has no recursive storage walk; its deletion calls are test-only. **No v3 walk**. |
| Inspect command | `crates/aft/src/commands/inspect.rs` | Same module in B, with diagnostics/rendering changes | No storage directory enumeration, unlink, recursive removal, rename or chmod in this command. Persistence is delegated to the inspect cache, not a walk of `blobs`. |
| Inspect cache open, old-generation deletion, scope inventory and age-policy GC | `crates/aft/src/inspect/cache.rs:565-647,1641-1650,1732-1749,1787-1986` | `:574-673,1677-1687,1768-1785,1823-2029` | Keyed `inspect/<scope>` trees only. Generation cleanup removes matching `<key>.g*.sqlite` and sidecars; scope GC sizes/removes children beneath its supplied inspect root (production eviction delegates to retention). **No v3 walk**. |
| Inspect manager file walk, view projection and non-view callgraph maintenance | `crates/aft/src/inspect/manager.rs:1503-1573,4526-4532,4983-5026,5190-5350,5570-5589` | `:1583-1666,4858-4866,5377-5480,5584-5775,5964-5985`; current-checkout projection now receives its file list | Project walk is rooted at the bound project and scope-filtered, not at storage. View projection uses `views/<scope>` and query pins; views-off maintenance uses `callgraph/<key>`. **No v3 storage walk** in ordinary routing. |
| Legacy partition inventory / sizing | `crates/aft/src/legacy_partitions.rs:202-244,281-358,447-479` | Same roots and key filters; lexical normalization now delegates to `path_identity` (including unmatched parent components) | Lists direct storage children, then only their `callgraph` and `inspect` partition-key entries; recursively sizes selected legacy partitions. **No v3 count/touch/delete**. |
| Private storage permission tightening | `crates/aft/src/private_storage.rs:87-147,150-195` | File unchanged byte-for-byte | Root opening tightens the root and immediate directory children only; keyed opening tightens only its ancestor chain. May chmod `blobs` itself, never descends to `blobs/v3`. **No v3 member/store mode change**. |
| Explicit migration preflight, copy and staging cleanup | `crates/aft/src/migrate_storage.rs:145-157,260-272,356-407,450-533,720-765` | `:145-157,265-277,361-411,455-539,725-770,842-907`; B groups free-space admission by destination volume | **Read-only count exception:** recursive source sizing enters v3 when `--from` contains it. Actual migration items exclude `blobs` and `views`; staging cleanup selects only `staging-*` under migration targets. No v3 migration/deletion in ordinary layouts. |

### Audit decision and explicit migration exception

Neither release's automatic startup, retention, GC, family sweep, eviction or
legacy pruning discovers, counts, changes permissions on or deletes the chosen
v3 family/member trees. The two releases differ in reclaim policy, measurement
caching, due-view scheduling and mounted-cache trash handling, not in their
v1/v2 namespace roots. This is a source audit, not the later actual-binary G7
coexistence test.

One user-invoked operation is an explicit exception: `aft migrate-storage
--from <storage>` recursively sizes the entire supplied source root, including
v3 (A `crates/aft/src/migrate_storage.rs:145-157,752-765`; B
`:145-157,757-770`). Its fixed migration list excludes blobs/views, so it
counts those bytes without moving them. The owner approved continuing with
this shipped-release exception: it cannot delete or corrupt v3; at worst its
space estimate refuses a migration that could have fit. Forged v2 `view_dir`
paths remain a corrupt-metadata caveat only, not an ordinary routing path.

**Completed in slice 3a-2:** current migration excludes the source-root-relative
`blobs/v3` subtree from **both** source-size/free-space accounting and recursive
copying. V3 is rebuildable and remains at the source. The change landed in
`2cb838f2436f7d22ad4aee18043085a1d93539b2` (through #675); the test in
`crates/aft/src/migrate_storage.rs` seeds an 8 GiB v3 payload, admits migration
with space for only the actual migration fixtures, and verifies that no v3
target is created and the source payload is unchanged. Accounting for actual
migration items and destination-volume checks is preserved. This current-main
fix does not change the explicit-migration exception in the two shipped
releases audited above.

## Floor domains and publication protocol

The permanent format names are `views_v3_registry` (family `members.sqlite`)
and `views_v3_store` (plane payload stores). Each has supported and written
format version **1**. Both belong to `PersistedStore::ALL` and therefore to
`aft --formats` and known-store floor refusals, but neither belongs to the
startup baseline. A fresh prepare writes only the pre-v3 stores; repeated
prepare must not add a v3 floor or create `blobs/v3`.

`reader_floor::publish_v3_with_floor(storage, registry_version, store_version,
write)` is the combined publication boundary. It calls `raise` once with both
requirements, then invokes the closure. The closure must reach `V3FloorRaised`
before creating any v3 file; that durability-observer integration belongs to
the later v3 writer. Every publication uses this boundary, even if the floor
already covers its versions. There is no first-write decision or pending flag.

`raise` holds the cross-process floor lock and preserves every existing store
name, including unknown names, taking a separate maximum for each requirement.
It never rewrites an uninterpretable floor. Changed floors use a temporary
file, file sync, rename and storage-root directory sync. A request naming either
v3 store is strict: a failed directory sync returns `FloorError::Io`, leaving
the closure uncalled. A rename may have already installed the higher floor
when sync fails. Consequently, an already-covered **strict** raise writes no
floor bytes but always re-syncs the storage root, propagating failure again.
A subsequent process can safely retry without relying on the previous
process's memory.

Requests naming no v3 store preserve the legacy behavior: an already-covered
raise performs no directory sync, and a changed floor attempts directory sync
but ignores every resulting error, including EIO, EACCES, EINVAL and ENOTSUP.
The startup baseline and old-store `publish_with_floor` therefore gain neither
a covered-request fsync nor a new directory-sync failure. Strict v3 requests
refuse all directory-sync errors. `durability::sync_dir` itself retains its
existing platform policy; the legacy exception belongs only to reader floors.

On non-Unix, `durability::sync_dir` remains a no-op `Ok`. **Non-Unix directory
durability is unverified**; a cross-compile is not a durability test. All
floor directory-fsync injection tests are `#[cfg(unix)]`.

## Registry and plane stores

Source: `crates/aft/src/blob_store/v3.rs` and `blob_store/v3/initialization.rs`.

| | Registry | Plane store |
|---|---|---|
| Path | `blobs/v3/<family>/members.sqlite` | `blobs/v3/<family>/<plane>.sqlite` (`semantic`, `callgraph`, `trigram`) |
| Persisted store name | `views_v3_registry` | `views_v3_store` |
| Format version / schema version | 1 / 1 | 1 / 1 |
| `PRAGMA application_id` | `0x41465452` (`REGISTRY_APPLICATION_ID`, "AFTR") | `0x41465450` (`PLANE_APPLICATION_ID`, "AFTP") |
| Layout tag (`format_meta.layout_tag`) | 0 | 2 (`LAYOUT_TAG`, new stores); 1 remains readable/writable without conversion |

Both stores carry the same metadata singleton:

```sql
CREATE TABLE format_meta (singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  format_version INTEGER NOT NULL, schema_version INTEGER NOT NULL,
  layout_tag INTEGER NOT NULL, schema_text TEXT NOT NULL);
```

Registry tables:

```sql
CREATE TABLE members (
 scope TEXT NOT NULL PRIMARY KEY, kind TEXT NOT NULL CHECK(kind IN ('view','base')),
 root BLOB, view_dir TEXT NOT NULL, registered_at_ms INTEGER NOT NULL,
 last_bind_ms INTEGER NOT NULL, last_publish_ms INTEGER,
 state TEXT NOT NULL, missing_sweeps INTEGER NOT NULL DEFAULT 0);
CREATE TABLE view_sessions (session_id TEXT NOT NULL PRIMARY KEY, scope TEXT NOT NULL,
 owner_pid INTEGER NOT NULL, owner_start INTEGER NOT NULL, opened_at_ms INTEGER NOT NULL);
CREATE TABLE reservations (id INTEGER PRIMARY KEY AUTOINCREMENT,
 owner_pid INTEGER NOT NULL, owner_start INTEGER NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('reserved','committed','released')),
 reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes>=0),
 charge_bytes INTEGER NOT NULL CHECK(charge_bytes>=0), completed_seq INTEGER);
CREATE TABLE accounting (singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 measured_bytes INTEGER NOT NULL, watermark INTEGER NOT NULL);
```

`members.view_dir` is always `blobs/v3/<family>/views/<scope>`, never under
`<storage>/views/`. A member's `root` is the checkout path's bytes, or NULL.

Layout-1 plane store tables (the original v3 layout):

```sql
CREATE TABLE store_meta (singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 plane TEXT NOT NULL, gc_epoch INTEGER NOT NULL);
CREATE TABLE blob_payloads (full_key BLOB NOT NULL PRIMARY KEY CHECK(length(full_key)=32),
 payload BLOB NOT NULL, payload_digest BLOB NOT NULL CHECK(length(payload_digest)=32),
 payload_schema INTEGER NOT NULL, ref_epoch INTEGER NOT NULL, created_at_ms INTEGER NOT NULL) WITHOUT ROWID;
CREATE INDEX blob_membership ON blob_payloads(full_key, ref_epoch);
CREATE TABLE blob_quarantine (full_key BLOB NOT NULL PRIMARY KEY CHECK(length(full_key)=32)) WITHOUT ROWID;
CREATE TRIGGER immutable_payload BEFORE UPDATE OF payload, payload_digest ON blob_payloads
 BEGIN SELECT RAISE(ABORT, 'immutable_payload'); END;
```

The payload column is **`payload`** and the digest column is
**`payload_digest`** (BLAKE3 of `payload`, checked again on every `get`).
`payload_schema` is preserved from the copy source.

### Schema identity

A new store is bootstrapped in one transaction: `auto_vacuum = INCREMENTAL`
(2) is selected first, then the schema, `application_id`, seed rows and the
final `format_meta` row. Every open then checks, and refuses through
`persisted_format` with `RefusalSource::Incompatible`:

- `format_version` readable (0 when not), and not newer than supported (a
  newer version keeps the `Artifact` refusal);
- `application_id` equals the kind's value (`application_id`);
- `layout_tag` selects an exact compiled schema (`layout_tag:<value>` for an unknown tag);
- `schema_version`, the stored `schema_text`, and the live `sqlite_schema`
  rows (type, name, tbl_name, sql, excluding `sqlite_%`) equal this build's
  compiled-in text (`schema_drift`), and the seed rows exist.

No table is created lazily; a later table bumps the schema version through
an exact-predecessor forward migration.

### PRAGMAs per open path

Each value is set and read back; a mismatch refuses as `pragma:<name>`.

| Open path | PRAGMAs |
|---|---|
| Bootstrap (creation, before the schema transaction) | `auto_vacuum=2`; `application_id` inside the transaction |
| Writer open (publication, collector, recovery) | `busy_timeout=5000`, `journal_mode=WAL` (must answer `wal`), `synchronous=2` (FULL), `foreign_keys=1`, `temp_store=2`, `wal_autocheckpoint=0`; `auto_vacuum` must read 2 |
| Ledger read (read-only) | `busy_timeout=5000`, `query_only=1`; `auto_vacuum` must read 2 |

## Layout tags and rowid payloads

`format_meta.layout_tag` is a per-file integer, not a new persisted format
name or an `aft --formats` version. `blob_store::v3_format_report(path)`
validates an existing file through the shared owning connection and returns
`store_name`, `format_version`, `schema_version` and `layout_tag`. It is a
read-only open: no lock, measurement, creation, migration or ledger write.
Registry files keep tag 0; plane files accept tags 1 and 2. Unknown tags are
refused through `persisted_format` as `layout_tag:<value>` before schema
comparison. Both layouts retain format version 1 and schema version 1; schema
identity is compiled for the pair (schema version, layout tag).

Only new plane files under `blobs/v3/<family>/` get tag 2. Existing tag-1 files
keep their schema and use layout-dispatched inserts and touches. There is no
converter, including on writer open. V1 and v2 paths, schemas, membership index
names and production routing are unchanged.

Tag 2 replaces the payload and membership tables above with:

```sql
CREATE TABLE blob_payloads (id INTEGER PRIMARY KEY,
 full_key BLOB NOT NULL CHECK(length(full_key)=32),
 payload BLOB NOT NULL, payload_digest BLOB NOT NULL CHECK(length(payload_digest)=32),
 payload_schema INTEGER NOT NULL, created_at_ms INTEGER NOT NULL);
CREATE UNIQUE INDEX blob_locator ON blob_payloads(full_key);
CREATE TABLE blob_liveness (
 blob_id INTEGER PRIMARY KEY REFERENCES blob_payloads(id) ON DELETE CASCADE,
 ref_epoch INTEGER NOT NULL);
CREATE INDEX blob_membership ON blob_liveness(ref_epoch, blob_id);
CREATE TRIGGER immutable_payload BEFORE UPDATE OF payload, payload_digest ON blob_payloads
 BEGIN SELECT RAISE(ABORT, 'immutable_payload'); END;
```

`store_meta` and `blob_quarantine` are unchanged. The narrow unique locator
keeps all 32 key bytes but no payload. Mutable liveness (`ref_epoch`) lives
only in `blob_liveness`: a reuse touch updates that small row, not a payload
record with overflow pages. Insert and liveness creation share one transaction;
DELETE of a payload remains allowed and cascades to liveness with the writer's
verified `foreign_keys=1`. The trigger refuses payload-only and digest-only
UPDATE, including an UPDATE which assigns the existing value. A ref_epoch
touch is instead an UPDATE of `blob_liveness` and succeeds.

Copy reuse reserves `TOUCH_RESERVATION` (64 KiB) in tag 2, independent of payload
length. Tag 1 retains row length plus its 1 MiB margin because its epoch still
shares the payload record. A row lost before the touch cannot be inserted
under the small reservation; the copier retries under a full copy reservation.
The changed reservation assertions replace the original shared-record budget
contract because the new layout no longer rewrites payloads on reuse.

### Storage-only lookup and open gates

Run `python3 scripts/views-performance-gate.py --rowid-layout --output <new-dir>`
on Linux. It selects the pinned slice-1 AFT corpus (`05d40ac417db6137a4204521bd3c59853325e968`)
and runs the ignored `blob_store::v3::layout_gate::rowid_layout_receipt` test.
The test populates both layouts by copy-on-first-use from a pinned `Cargo.lock`
payload, repeated to 1 MiB per row, in identical 32-row insert order. This is
a composition of two fixtures preserved in `ffbbea3e0`: its digest SELECT
with 256 probes and its shared-31-byte-prefix construction (`[7;32]`, final
byte 0..63 repeated four times). Each batch has 128 hits and 128 misses.
It does not reproduce the separate real-snapshot materialization timing
mentioned in that commit's message.

Each arm is opened fresh after population and byte-parity checks. One warmup
batch is discarded; the median is taken over 11 batches. Receipts preserve
process CPU and wall nanoseconds and the lookup statement's VM-step/full-scan
counters in every batch, physical locator-tree overflow page counts from
`dbstat`, and SQLite version/source ID. SQLite 3.46.0 is pinned by the baseline.
The layout-1 primary tree's physical `dbstat` name is `blob_payloads`, not the
logical `sqlite_autoindex_blob_payloads_1` name.

The first baseline commit records unchanged layout 1 in
`scripts/views-performance-baselines/slice-3b-linux-x86_64.json`. The paired
Linux acquisition measured 89.551 ms versus 2.121 ms median wall time (**42.23x**),
8224 versus 0 locator overflow pages, and 2176 versus 2560 VM steps per batch
(full-scan steps 0 in both). Every key's `get` returned byte-identical bytes in
both layouts. This is a storage-only result, not an end-to-end daemon claim.

The harness also runs the ignored
`blob_store::v3::open_tests::old_store_dispatch_and_copy_readers_never_migrate_on_open`:
a real, checkpointed v2 database exceeds 1 GiB and has no WAL on entry, and a
v1 fixture already has `created_at_ms`. Writer/read-only v3 dispatch refuses
both without migration; the v1/v2 copy-source readers open them unchanged.
Schema cookies and page counts are equal, and a connection-installing SQLite
statement tracer reports zero schema-capable statements and zero payload
writes. It conservatively includes no-op DDL; its control executes real DDL
and payload writes and must count them. Empty sidecars which SQLite's read-only
WAL reader can leave are not payload writes.

`layout_tests` cover each immutable column separately, allowed epoch touch and
GC DELETE, format reporting on both layouts and the registry, and the multi-MiB
reuse WAL peak. SQLite's WAL hook measures frames before publication checkpoints
them: an 8 MiB payload in the old shared-record layout peaks at 8,495,472 bytes;
the rowid layout must stay below 256 KiB. The old-layout control must exceed
8 MiB, proving that the hook observes the expensive write rather than the
post-checkpoint empty file.

## Ledger, lock and open modes

- Reservation rows live in each family's v3 `members.sqlite`. A row moves
  `reserved` → `committed` on successful publication, or `reserved` →
  `released` on failure, each in one transaction. It records the owner's pid
  and process start time.
- The storage-root reservation lock is `blobs/v3/reservations.lock`, an
  `fs_lock` lease (dead holder or a heartbeat older than 15 s is taken over).
  Lock order is always this lock first, then the family registry barrier (an
  IMMEDIATE transaction). The lock is not reentrant: a holder passes its
  `ReservationLock` to nested writers (`publish_locked`).
- **Writer open** (`RegistryWriter::open_existing`, and publication's private
  creating open): under the lock, then the barrier; takes a covering
  measurement and releases rows whose owner is dead.
- **Ledger read** (`LedgerReader::open_existing`): opens an existing registry
  read-only, takes neither the lock nor the barrier, takes no measurement and
  writes nothing. Used for the floor sum over other families and for reports.
- Only publication creates a registry. Recovery and the retention collector
  open existing registries only.

## Accounting

- The namespace inventory is every file under `blobs/v3/<family>/`: the
  registry, plane stores, their `-wal`/`-shm`, and the member view
  directories. Sizes are allocated sizes (`st_blocks * 512`); directories are
  walked, not charged.
- A covering measurement checkpoints the registry (`wal_checkpoint(TRUNCATE)`),
  records the inventory size in `accounting.measured_bytes` and the watermark
  (the last `completed_seq`), and repeats until recording it does not change
  the inventory. It runs on every writer open, after every collector pass and
  collector deletion, and on every release.
- Committed usage = last measurement + charges of reservations committed after
  its watermark + bytes of outstanding `reserved` rows.
- On commit, the measured growth of the inventory during the attempt (never
  less than the payload bytes written) replaces the reserved bytes, in the
  transaction that publishes the closure.
- On failure (owner alive or dead) one transaction takes a covering
  measurement, which includes plane rows already committed, and releases the
  row. Orphan plane rows stay charged until they are reclaimed.
- A measurement taken under a deadline (`covering_measurement_until`,
  `open_existing_until`) checks it before each directory entry of the walk.
  If the deadline passes, the walk stops, its transaction is rolled back, and
  nothing is recorded or released: the result is `Measurement::Owed`. A
  partial sum is never recorded as a total. A walk that completes is
  confirmed only by a complete recheck finding the same total, because the
  measurement's own commit and checkpoint can allocate registry pages. If the
  deadline stops that recheck, the result is also `Owed`, and the saved total
  is raised by the growth of the registry's own files since the walk counted
  them (found by stat, not a walk), so it is never below what the family
  uses. The registry keeps its last
  complete measurement, which can only overstate usage after deletions, and
  every writer open measures again, so the next writer or the collector's
  next visit repays it. The retention collector uses its pass deadline for
  every measurement and reports an owed one in the pass report's
  `skipped_families` as `blobs/v3/<family>: measurement_owed`.
- Deletions lower committed usage only through the next covering measurement,
  by the allocated bytes actually freed. Plane row deletions count once their
  pages are reclaimed or the WAL is checkpointed; v3 plane stores use
  `auto_vacuum = INCREMENTAL` so `reclaim_space_bounded` can shrink them.

## Budgets and floor

- Namespace budget per family: 4 GiB (`NAMESPACE_BUDGET`, the family hard
  limit) until it is measured. A reservation is refused as
  `v3_namespace_budget` when committed usage plus the request exceeds it.
- Free-space floor: refused as `device_free_space_floor` when free space,
  minus the request and every outstanding `reserved` row in all
  `blobs/v3/*/members.sqlite`, would fall below max(4 GiB, 5% of capacity).
  Free space and capacity come from `statvfs` (`f_bavail*f_frsize`,
  `f_blocks*f_frsize`). Other families are summed through ledger reads; a
  registry that cannot be opened or read refuses as
  `reservation_ledger_unreadable`.
- The check runs before anything is created, and again under the lock after
  the floor raise. A refusal at the first check leaves `blobs/v3/` and
  `reader-floor.json` untouched; at the re-check it may leave `blobs/v3/`, the
  lock file and an empty registry, but no reservation row or plane store.
- Assumptions: different storage roots on one device are not coordinated. On
  Windows the probe is `GetDiskFreeSpaceExW` (caller-available bytes and
  total bytes of the nearest existing ancestor); other non-unix targets
  report unsupported, so every reservation there is refused as
  `device_free_space_floor`. The Windows probe is compile-checked only so
  far; a native Windows run is still owed.
- `DiskBudget::admit_v3`/`extend_v3` (`views/eviction.rs`) admit v3
  reservations against the family hard limit (refused as
  `storage_gap:v3_family_hard_limit`, never a legacy fallback) and flag
  usage above the 2 GiB target so the caller runs the family's v3 sweep.

## Retention collector

`storage_retention::collect_one_v3_family` runs in every retention pass, after
the v1 and v2 blob lanes. It visits one family directory under `blobs/v3/`
per pass through its own cursor (`retention/cursors/families-v3.json`), and
starts the next lap in the same pass so a lone family is visited every pass.

- **Existing registries only.** A root without `blobs/v3/` lists nothing and
  writes no cursor. A family directory without `members.sqlite` is skipped
  before the reservation lock is taken, because taking it creates
  `blobs/v3/reservations.lock`. Then: lock, `RegistryWriter::open_existing`.
- **v2's missing-root rule** (as `gc/family.rs` `deregister_if_missing`). A
  `kind = view` member counts a missed pass when its root is certainly missing,
  past the recency guard (`ROOT_GRACE_MS`, or the linked-worktree margin) and
  the mount guard (`missing_root_eligibility`), and it has no protection: no
  `view_sessions` row with a live owner, no live pin in `pins/`, and no
  protected read marker in `readers/`. Session rows, pins and markers whose
  owner is dead are removed on the way and never block collection. An
  unreadable pin or a `pins/`/`readers/` listing over its bound (4096) is
  uncertainty: the member stays and its count is not advanced.
- `missing_sweeps` must reach `MISSING_ROOT_SWEEPS` (2); a reappeared root or
  any live protection resets it to 0. Members with a NULL root and
  `kind = base` members are never eligible. A row whose `view_dir` is not
  `blobs/v3/<family>/views/<scope>` is kept and reported.
- **Deletion.** Under the held lock, one barrier transaction rereads the row
  and rechecks kind, root, recency, mount, count and every protection, then
  commits `state = 'evicting'`. The collector then removes the view directory
  (refusing while a SQLite connection in the process is open under it),
  deletes the member row, and takes a covering measurement. A crash leaves an
  `evicting` member, which the next pass finishes. Once `evicting` is
  committed the eviction is always finished, even if the root has come back.
- **The `evicting` contract for binders.** A binder that finds its member in
  `evicting` must never use that view directory. It treats the member as
  absent and publishes a fresh one, or waits for the eviction to finish.
  Finishing an eviction after the root returns is safe only under this rule.
- **Bounds** match `collect_one_v2_family`: a `PASS_BUDGET` (5 s) deadline and
  at most `HISTORY_ENTRIES` (8192) members per pass. Plane rows are left for
  slice 4's v3 plane sweep.
- **Resume position.** Each visit saves the scope of the last member it
  examined (`retention/cursors/v3-members/<family>.json`). The next visit
  takes members in scope order after it and wraps to the start at the end, so
  members that stay live, protected or slow cannot keep later members from
  being examined.
- **The lock wait shares that budget.** The collector takes the reservation
  lock with `ReservationLock::acquire_until(storage, deadline)`, using the
  family's pass deadline, never the 120 s wait of `acquire`. When another
  process holds the lock past that deadline, the family is skipped for this
  pass and listed in the pass report's `skipped_families` as
  `blobs/v3/<family>: reservation_busy`. The cursor is not advanced, so the
  next pass tries that family first.

Tests (`storage_retention::v3_collector_tests`):
`v3_collector_applies_the_missing_root_rule_to_view_members_only`,
`v3_eviction_interrupted_after_its_evicting_commit_is_finished_next_pass`,
`collecting_a_view_member_lowers_committed_usage_by_its_allocated_bytes`,
`v3_collector_never_creates_a_registry`,
`v3_collector_skips_a_family_whose_reservation_lock_is_busy`,
`v3_collector_resumes_after_the_last_examined_member`,
`v3_measurement_past_the_deadline_records_nothing_and_is_owed`,
`blob_store::v3::tests::a_recheck_stopped_by_the_deadline_never_saves_a_total_below_usage` and
`no_production_path_writes_v3_over_a_v0_60_0_floor`.

## Replay prototype

The replayed publication is the copy closure: v3 plane-row inserts or
touches, `ClosureFlushed`, then the registry transaction from `reserved` to
`committed`. `blob_store/v3/replay.rs` wraps SQLite's platform VFS and records
what a power cut would lose (unsynced writes and truncations, files created
since their directory's last sync, unsynced deletions); each case cuts power
before every mutating call in turn, restores the last synced state and
reopens the stores.

Invariant after every cut: the registry shows no row for the attempt, or
`reserved`/`released` claiming no rows, or `committed` with every copied row
passing `get` with a matching digest.

Result: passing. Tests in `blob_store::v3::replay_tests` (unix only):
`replayed_copying_closure_is_whole_or_absent_after_any_power_cut`,
`replayed_reuse_only_closure_is_whole_or_absent_after_any_power_cut`,
`replayed_retry_after_a_failed_floor_directory_sync_is_whole_or_absent`, and
the red control `red_control_without_the_closure_flush_a_power_cut_tears_the_copy`
(without `ClosureFlushed`, a replay shows `committed` with a missing row).

## Release isolation

`crates/aft/tests/integration/v3_isolation.rs` runs the binary built from tag
v0.60.0 (commit `988f4f3e52b9`) over a storage root holding a v3 tree
(`members.sqlite` with `-wal`/`-shm`, a plane store, and
`views/<scope>/` with `pins/`, `readers/` and a pointer database) next to a
v1 and a v2 store holding collectable rows. Both the freshly started and the
already-running case wait for a retention pass and an enforcement pass after
the seed, then run `aft cache prune-legacy` dry and with `--yes`. The second
configure of each case flips `format_on_edit`: v0.60.0 queues the configure
maintenance that schedules retention only when the effective configuration
changed, and answers an equivalent configure without it. The digest
of bytes and `st_mode` over `blobs/v3/` must be unchanged, and no v3 member
may appear in a v2 registry. The tests are `#[ignore]`d (they take seven and
thirteen minutes) and need `AFT_V0_60_0_BINARY`.

## Base-reference protocol (registry 2/2)

Source: `views/base_refs.rs`, `views/base_sweep.rs`, `views/base_builder.rs`.
A 1/1 registry is upgraded in one barrier transaction, only by a writer that
first raised the `views_v3_registry` floor to 2
(`blob_store::v3::open_base_refs_registry`). Registries created by the copy
path stay 1/1 until then. The added schema text is:

```sql
CREATE TABLE base_refs (worker_scope TEXT NOT NULL, base_scope TEXT NOT NULL, nonce TEXT NOT NULL,
 membership_digest TEXT NOT NULL, worker_generation TEXT,
 state TEXT NOT NULL CHECK(state IN ('intent','orphaned','published')),
 owner_pid INTEGER NOT NULL, owner_start INTEGER NOT NULL, updated_epoch INTEGER NOT NULL,
 PRIMARY KEY(worker_scope, base_scope, nonce));
CREATE INDEX base_refs_by_base ON base_refs(base_scope);
CREATE TABLE base_builds (base_scope TEXT NOT NULL PRIMARY KEY,
 owner_pid INTEGER NOT NULL, owner_start INTEGER NOT NULL, owner_token TEXT NOT NULL,
 attempt INTEGER NOT NULL CHECK(attempt>=1),
 state TEXT NOT NULL CHECK(state IN ('building','serving')), heartbeat_at_ms INTEGER NOT NULL);
CREATE TABLE strict_rebuild (worker_scope TEXT NOT NULL, worker_generation TEXT NOT NULL,
 reason TEXT NOT NULL, recorded_epoch INTEGER NOT NULL, PRIMARY KEY(worker_scope, worker_generation));
CREATE TABLE base_sweeps (base_scope TEXT NOT NULL PRIMARY KEY,
 unreferenced_sweeps INTEGER NOT NULL DEFAULT 0);
CREATE TABLE sweep_state (singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 gc_epoch INTEGER NOT NULL, lease_pid INTEGER, lease_start INTEGER, reconcile_cursor TEXT,
 dangling_base_edges INTEGER NOT NULL DEFAULT 0, live_base_cap_hits INTEGER NOT NULL DEFAULT 0);
```

- **Identity.** The membership digest is BLAKE3 over the length-prefixed
  tag `aft/views-v3/base-membership/1`, the entry count, then per path (in
  path order) the path bytes, an entry-kind byte, and mode, content hash,
  size and resolution-input flag (regular files), target bytes (symlinks),
  OID (gitlinks) or name (synthetic entries). Readiness is excluded. A
  manifest with `head_tree`, any `git_oid`, a `base` header, tombstones or
  overrides is not a base. The identity digest (tag
  `aft/views-v3/base-identity/1`) covers family, all producers, the ignore
  fingerprint, the platform family, `head_tree:none` and the membership
  digest. The base scope is `base-m1-<identity hex>`.
- **Authentication.** Every consumer resolves a base by pin-then-verify and
  recomputes both digests from the pinned manifest; a `BaseHeader` is
  checked against that result, never trusted. `bound_generation` is
  diagnostic.
- **Markers.** A `BaseRefMarker` is a read marker in `M/readers/<G>/`, where
  `M` is the base's member directory and `G` its current generation, whose
  file also carries a per-acquisition `nonce` (omitted, and so unchanged,
  for ordinary readers).
- **Sweep.** Runs under the reservation lock; marks current, read-marked
  and pinned generations; reconciles at most 256 rows / 25 ms per pass from
  a wrap-around position kept in `sweep_state.reconcile_cursor`; reclaims a
  base after two consecutive unreferenced sweeps. The plane-row sweep keeps
  its resume position at `retention/cursors/v3-plane-rows/<family>.json`
  (format 1) when the retention collector runs it.
- **Segments.** v3 generations carry no trigram segment: v3 has no segment
  rows, files or segment collection. A reader assembles the base
  generation's segment in memory from the base's stored trigram payloads
  (shared through the segment cache while an index holds it) and loads the worker's changed
  entries as the usual overlay. No new on-disk rows were added for this.
- **Derived graphs.** A base's derived call graph is
  `M/derived-<generation>-a<attempt>-<owner_token>.sqlite`, named by the
  existing `M/derived-<generation>.ref` owner-reference file, and is built
  only once every callgraph member of that generation is ready. A worker's
  derived graph covers its composed generation and lives in its own member
  directory under the usual `derived-<generation>.sqlite` name; while the
  base's graph is pending the worker has none.

## Deferred implementation register

This register assigns the remaining formats **before their owners write them**.
These are forward format assignments, not a claim that the schemas or files
already exist. No 3a or 3b implementation is deferred here. Storage paths below
are relative to `<storage>`; implementation-source paths are repository-relative
(`gc/family.rs` abbreviates `crates/aft/src/gc/family.rs`). Define
`F = blobs/v3/<family>` and `M = F/views/<scope>` for either a base or a worker.
No storage artifact is assigned beneath `<storage>/views/v3`. The opening
contract is [views-slice4-opening.md](views-slice4-opening.md).

### Versions and upgrade boundaries

- The implemented registry remains `views_v3_registry` format/schema **1/1**.
  Reserve **2/2** for slice 4's base-reference protocol, **3/3** for 4v's token
  and stat-evidence protocol and **4/4** for slice 5's donor-seal protocol.
  Version 5/5, earlier reserved for slice 6a's symbol and import protocol, is
  **not used**: the import ledger is a sidecar file and the symbol plane is an
  ordinary plane store (see *Import ledger and symbol plane* below), so neither
  changes the registry schema. These versions gate the whole member
  protocol, including its sidecars; a version can advance without adding a
  registry table. An owning slice
  installs its complete schema through an explicit versioned upgrade, never a
  lazy `CREATE TABLE` on read. Later versions preserve the earlier contracts;
  an older reader refuses an unsupported version rather than interpreting it.
- `views_v3_store` remains format/schema **1/1**, with the existing layout-tag
  dispatch (new stores use layout 2). Sweeping rows and adding a symbol-plane
  instance do not change the payload/digest schema or convert existing stores.
- A v3 generation manifest uses **manifest format 3**, distinct from legacy
  manifest format 2. Its path retains the existing
  `manifest-<generation>.json` naming convention but lives only in `M`. Sidecar
  formats below start at **1** and are not new `PersistedStore` names. Local
  artifact versions are independent of registry/floor numbers: the manifest
  records `format_version: 3`; new JSON sidecars and the plane-sweep cursor
  record `format_version: 1`; stat-evidence SQLite records format/schema 1/1
  in its `format_meta` singleton. The staging-name protocol's version 1 is a
  naming contract, not a file header, and is identified by registry version 2.
  Registry and plane format/schema numbers remain in their existing
  `format_meta`; reader-floor requirements remain in `reader-floor.json`.
  An incompatible artifact change requires a new local version and a
  registry-floor version that makes unsupported readers refuse.
- Every artifact publication uses the combined v3 floor boundary. Slice 4
  requests registry floor 2 before its artifacts; 4v requests 3 before stat
  evidence or tokens; 5 requests 4 before donor/seal publication. Slice 6a
  writes its symbol plane and import ledger only inside
  `blob_store::v3::publish`, which requests registry floor 1 and store floor 1
  before creating either file. Every publisher also requests store floor 1.
  These are future written versions, not changes to today's startup baseline.

### Assigned paths and owners

| Owner | Item and path | Assigned format / schema | Required contract |
|---|---|---|---|
| 4 | `M/manifest-<generation>.json`: optional `base` header and `PublishedFrontier` | Manifest 3, registry floor 2 | `base` records scope, membership digest, identity digest and diagnostic bound generation. The frontier records delta cut version, watcher epoch, publication time and optional verified `{E0, F, delivery_seq}`. No separate binding file; no base-reference chains. |
| 4 | `entries`, `tombstones`, `readiness_overrides` in that same manifest | Manifest 3, registry floor 2 | Sections are disjoint. Tombstones mask inherited deletions. Override Ready keys participate in `ready_keys()`, marking and live-pin trimming. Slice-4 identity is membership-keyed, `head_tree=None`, with no proven `git_oid`. |
| 4 | `F/members.sqlite`: `base_refs`, `base_builds`, `strict_rebuild`, base unreferenced counts, sweep epoch and reconciliation resume state | `views_v3_registry` 2/2 | Exact-generation edges, per-acquisition nonce, owner-token/attempt CAS, bounded epoch-aware reconciliation and two-sweep reclaim. A live claim holds an empty base. Published and intent rows have explicit terminal cleanup. |
| 4 | `M/staging/a<attempt>-<owner_token>-<nonce>/`; `M/derived-<generation>-a<attempt>-<owner_token>.sqlite` and associated temporary/WAL/SHM names | Staging-name protocol 1, registry floor 2 | Every base-build staging and derived name carries the attempt and owner token. Never reuse a dead attempt's name. Reclaim only after its owner is known dead; a live claim is not abandoned work. |
| 4 | Plane-row sweep over `F/{semantic,callgraph,trigram}.sqlite`; progress at `retention/cursors/v3-plane-rows/<family>.json` | Store 1/1; cursor 1; registry 2/2 for sweep coordination | Implement in `gc/family.rs` and `storage_retention.rs`. Mark the composed depth-one closure, including overrides, then delete only unmarked eligible rows under the epoch/lease protocol. Bound the producing iterator and deadline; resume rather than treating a partial scan as complete. Include later registered planes when they exist. |
| 4 | `DiskBudget` wiring in `crates/aft/src/views/eviction.rs`, using `F/members.sqlite` reservations/accounting and the existing namespace inventory | Registry 2/2; no separate budget file or new config format | Preserve allocated-byte charging, the 2 GiB family target, 4 GiB hard/namespace limits and max(4 GiB, 5%) device floor. Refuse with the named storage gap and no legacy fallback. |
| 4v | `M/stat-evidence-<generation>.sqlite` | Stat-evidence 1/1, registry floor 3 | Generation-paired path/root/file identity, size/times, stable content digest, proven Git OID if any, observation-time conversion-policy digest and per-plane keys. No stat-cache or current-policy-only proof. |
| 4v | `F/members.sqlite`: `view_tokens` rows | `views_v3_registry` 3/3 | Single-use advisory token bound to identity at first consumption, with expiry and consume state. Stored in daemon storage, never the checkout. Stat evidence and tokens do not replace freshness validation. |
| 5 | `M/donor-<generation>-<fingerprint>.seal.json` beside `M/donor-<generation>-<fingerprint>.sqlite` | Donor seal 1, registry floor 4 | Atomically publish fingerprint, size and `journal_mode=DELETE` only after the detached donor is checkpointed and synced and has no WAL/SHM. A published manifest names only a completed seal. Never copy/clone a live SQLite file set. |
| 6a | `F/symbol.sqlite` | `views_v3_store` 1/1, new-store layout 2; store floor 1 | The v3 symbol plane uses the same immutable `payload`/`payload_digest` contract and reservation protocol. No in-place legacy conversion. Implemented; see *Import ledger and symbol plane*. |
| 6a | `F/imports.sqlite` (sidecar, not a registry table) | Import ledger 1/1, `application_id` `0x41465449`; registry stays 1/1 or 2/2 | Record completion/rejection per legacy key, artifact, compatible producer and destination plane, with owner-token claim/recovery semantics. Import into v3 only, stream inputs above 1 GiB and retain legacy bytes. Implemented; see *Import ledger and symbol plane*. |

#### Stat evidence and advisory tokens

Registry 3/3 is an exact-predecessor upgrade of 2/2 under the v3 reservation
barrier, after raising only `views_v3_registry` to 3 and `views_v3_store` to 1.
It adds `view_tokens(nonce TEXT PRIMARY KEY, root BLOB NOT NULL,
expected_tree TEXT NOT NULL, minted_ms INTEGER NOT NULL, expires_ms INTEGER NOT NULL,
bound_root BLOB, bound_gitdir BLOB, consumed_ms INTEGER)`. Root is the canonical
future checkout path. The two bound identity blobs and consume time are all
NULL at mint and all set in one first-consumption transaction. A consumed row
is retained until expiry so replay and identity mismatch can be distinguished;
neither can authorize verification or publication. The lifetime is ten minutes.
Identity blobs are versioned JSON, not a Git stat-cache tuple. Older registry
readers supporting at most 2 refuse registry 3; v1/v2 store formats are unchanged.

Stat-evidence 1/1 uses `application_id=0x41465445`, `PRAGMA user_version=1`, and
`evidence_meta(singleton INTEGER PRIMARY KEY CHECK(singleton=1),
format_version INTEGER NOT NULL, schema_version INTEGER NOT NULL,
generation TEXT NOT NULL, root BLOB NOT NULL, policy TEXT NOT NULL)` plus
`stat_evidence(path BLOB PRIMARY KEY, record BLOB NOT NULL)`. Records are
versioned JSON containing root/file identity, size, mtime, ctime or Windows
ChangeTime, observation time, content digest, an optional proven Git OID,
the policy digest paired with that observation, and per-plane keys. The file
is staged and synced before the generation pointer CAS and is reusable only
when its generation, root, policy, file identity, timestamps and racy checks
all agree. Missing, corrupt or future evidence means reading source again.
Evidence does not upgrade membership-keyed bases to tree-keyed identities.

Evidence staging is `M/.stat-evidence-<generation>.sqlite.tmp.<nonce>` (and
SQLite's `-journal`, `-wal`, `-shm` suffixes if present), so a crashed write has
an attributable generation. The prepared-generation pin is acquired before
the manifest or evidence is exposed and remains held through publication.
The existing generation sweep discovers the permanent database and these
temporaries and retires them in the same pin-checked claim as the generation;
its existing moved-file/unlink accounting includes their bytes. Current and
pinned generations retain evidence and staging regardless of age. Abandoned
staging is removed only when its generation retires, never by a second sweep
or by a timer that can bypass a live pin.

Policy capture uses counted `git config --list --show-origin -z` **and**
`git var GIT_ATTR_SYSTEM` / `git var GIT_ATTR_GLOBAL`, rather than guessing
an installation prefix for attribute sources. This supersedes the earlier
config-only query wording in the views design. Every successful spawn,
including an unsupported-variable exit, contributes to `git_subprocesses`
in the process work counters. Either source query failing disables stat reuse
and takes strict reconciliation. Measurements of these additional queries
belong to the later performance gate.

The registry-version increments above are serial handoffs, not permission for
an earlier slice to write a later owner's rows. Owners implement and test their
assigned schema and encoding before publishing any such bytes. Registration
does not enable a production path: unrouted entrypoints remain reachable only
through the marker-gated harness. Existing cursor paths for family discovery
and member retention remain unchanged.

Slice 4's binder must honour the `evicting` contract above: when it finds its
member in `evicting`, it never uses that view directory, and either treats the
member as absent and publishes a fresh one or waits for the eviction to finish.

## Import ledger and symbol plane

Sources: `crates/aft/src/migration/v3_import.rs` (decoders, staging,
registration), `crates/aft/src/migration/v3_ledger.rs` (ledger and driver),
`crates/aft/src/migration/v3_symbols.rs` (symbol importer) and
`crates/aft/src/views/symbol_store.rs` (symbol plane). Imports write only
under `blobs/v3/<family>/` and their own staging directory; they never write
a v1 or v2 row, manifest, pointer or schema, and never move, change or delete
a legacy file.

### Symbol plane `F/symbol.sqlite`

- An ordinary v3 plane store (`views_v3_store` 1/1, layout 2, `store_meta.plane
  = 'symbol'`), opened and written through the same `blob_store::v3`
  connections, `Publication` and `PlaneReader` as the other planes:
  `FamilyPlane::Symbol`. It is a v3-only plane: it is not in
  `FamilyPlane::ALL` (which manifests, v2 stores, v2 GC and eviction
  iterate) but is in `FamilyPlane::V3_STORES`, which the v3 plane-row sweep
  and `v3_format_report` use. No manifest entry records symbol readiness.
- Key: BLAKE3 over the tag `aft/views-v3/symbol/v1`, then the content hash
  (32 bytes), the language id (the parser's `LangId` name, for example
  `Rust`) and the producer id, each preceded by its
  length as a big-endian u64. The producer id is
  `aft-symbols/extraction-<SCHEMA_VERSION>` (`symbol_cache_disk::SCHEMA_VERSION`,
  bumped on any grammar, query or extractor change), so a parser or schema
  change produces a different key and misses. A source hash alone is never
  a key.
- Payload (payload schema 1): `u8` layout 1, then three length-prefixed
  (`u32` LE) fields: producer id, language id, and the symbols as the JSON
  array `symbols.bin` stores. A reader checks the layout, producer and
  language against its key's inputs before decoding.
- **GC.** Manifests never name symbol rows, so the sweep does not use marking
  for them: it deletes a symbol row only when its `ref_epoch` is more than
  `SYMBOL_RETENTION_SWEEPS` (8) sweep epochs old. A lookup that uses a row
  touches it inside a publication, which restamps it.
- **Residency.** Decoded symbol lists are shared as `Arc<Vec<Symbol>>`
  through a process-wide cache in `views/symbol_store.rs`, bounded at 64 MiB
  of estimated bytes; eviction from it never touches stored rows.
- **Floor and accounting.** Written only inside `v3::publish` (registry floor
  1, store floor 1, a reservation per batch); its bytes are part of the
  family's namespace inventory like any plane store.

### Import ledger `F/imports.sqlite`

A sidecar SQLite file, not a registry table: `members.sqlite` and its
version are unchanged. It is created, and every row transition is written,
inside a `v3::publish` call (64 KiB reservation), so the file exists only
after the v3 floor is raised and each write holds the reservation lock.
Readers (status, prune eligibility, retention) use this process's ledger
connection when it has one; otherwise they copy the database and its WAL to a
private temporary directory and read the copy, so a read never writes under
the storage root. `aft cache prune-legacy` reads `members.sqlite` the same way.

```sql
CREATE TABLE format_meta (singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 format_version INTEGER NOT NULL, schema_version INTEGER NOT NULL, schema_text TEXT NOT NULL);
CREATE TABLE imports (legacy_key TEXT NOT NULL, artifact TEXT NOT NULL,
 producer TEXT NOT NULL, plane TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('claimed','staged','validated','registered','done','rejected')),
 attempt INTEGER NOT NULL CHECK(attempt>=1), owner_pid INTEGER, owner_start INTEGER, owner_token TEXT,
 source_hash BLOB, source_len INTEGER, staged_hash BLOB,
 imported INTEGER NOT NULL DEFAULT 0, skipped INTEGER NOT NULL DEFAULT 0,
 reason TEXT, updated_ms INTEGER NOT NULL, completed_ms INTEGER,
 PRIMARY KEY(legacy_key, artifact, producer, plane));
```

Format/schema 1/1, `application_id` `0x41465449` ("AFTI"), WAL,
`synchronous=FULL`. A newer format is refused by name. `artifact` is
`trigram` (`index/<key>/cache.bin`), `semantic` (`semantic/<key>/semantic.bin`)
or `symbols` (`symbols/<key>/symbols.bin`); `producer` is the trigram policy
fingerprint, the semantic producer id or the symbol producer id; `plane` is
the destination plane. A row completes only its own (artifact, producer,
plane): a semantic-only completion never suppresses a trigram or symbol
import, and a producer change starts a new row.

- **States.** `claimed → staged → validated → registered → done`, or
  `rejected{reason}` from any non-terminal state. Each transition is one
  `BEGIN IMMEDIATE` compare-and-set on the row's state and owner token. A
  live owner makes other callers back off; a dead or released owner's row is
  taken over with `attempt + 1` and resumes from its durable state,
  repeating at most one idempotent step.
- **staged:** the canonical conversion is streamed to
  `<storage>/migration/v3/<family>/<legacy_key>/<artifact>-<plane>-<staged
  hash prefix>.bundle` (temp, fsync, rename, directory sync), with the
  legacy source's BLAKE3 and length recorded. Old binaries never look under
  `migration/v3`. A bundle is a magic `AFTIMP03`, the artifact byte, entries
  (`u8` 1, path, content hash, size, key, payload), then `u8` 0, the entry
  count and a BLAKE3 of every earlier byte.
- **validated:** the bundle is re-read and every key re-derived, and the
  legacy source is re-hashed and must still be the converted bytes.
- **registered:** every payload is in the destination plane, written in
  batches of at most 32 MiB, each one `v3::publish` with a reservation of
  twice its bytes plus 1 MiB. A refused reservation reports the plane's
  named storage gap `storage_gap:<refusal>` (`v3_namespace_budget`,
  `device_free_space_floor`, `reservation_ledger_unreadable`); the row keeps
  its state with the owner released and the reason recorded, so a later
  run resumes. Nothing falls back to a legacy reader.
- **done:** the staging bundle is removed.

Inputs have no size limit. The decoders stream: the trigram `cache.bin`
is checksummed in one sequential pass, its postings are counted per file in
one pass over the posting blob, then inverted in file-id ranges whose
postings fit a working-set budget (64 MiB by default), one more pass per
range; `semantic.bin` is decoded one file's rows at a
time. Memory is bounded by the per-file tables (path, size and hash per
file) plus the budget, never by the input size. The `semantic.bin` decoder
is read-only and refuses trailing bytes (appended segments or a torn tail)
rather than repairing or folding them; a path whose rows are not
contiguous is skipped and left for the plane to embed.

Imported trigram and semantic rows are not named by any manifest until a
base or worker build reuses them (`contains`, then `put_or_touch`). A v3
plane-row sweep that runs between the import and that build may collect
them; the build then recomputes those files. Imported symbol rows follow the
symbol plane's epoch rule above.
