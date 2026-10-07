# View publication: payload reads and rollback journals

## Membership is not payload consumption

The publication candidate walk used `BlobStore::get` to decide whether to extract a callgraph blob already present in a repository family. `get` selected the entire payload, allocated its bytes, and validated its BLAKE3 digest. A new worktree has no previous manifest, so the same-key shortcut against the previous generation cannot help it. This made the walk read every shared callgraph payload, only to discard it.

`BlobStore::contains` probes only the key through the existing covering `blob_membership(full_key)` index. The SQL explicitly selects that index because `blob_payloads` is WITHOUT ROWID: its primary-key tree contains the payload. No new index is created at store open. In particular, an index on `(full_key, payload_schema)` is avoided: `payload_schema` follows `payload` in the SQLite record, so constructing it on an existing store would traverse payload overflow chains across the whole table.

All production presence-only callers of the original `BlobStore::get` were changed:

- `views::assembly::missing_callgraph_payload`: avoid extraction/serialization/no-op puts for cached keys.
- `views::assembly::prepare_checkout`: the read-only publisher's availability recheck after candidate assembly (another writer may have filled a missing key).
- `subc::blob_store::BoundBlobStore::put`: idempotent reuse at the family quota boundary.

The remaining production callers consume bytes and keep verification: `commands::configure::semantic_view_blob_for_path` and `BoundBlobStore::get` (a forwarding reader). Migration's original BlobStore uses puts, not presence-only gets. FamilyStore v2 already has a separate membership API; its readers (including `views::semantic_arena::SemanticArena::load`), and the migration checks that decode payloads, are not converted into presence checks. Test assertions of blob contents/integrity remain reads.

### Integrity policy

Membership does **not** promise that a payload is uncorrupted or has a valid schema field. Both schema and digest validation happen when bytes are consumed. `get` retains its digest/schema checks and rejection-as-miss behavior. No permanent verified bit is used: it would not detect corruption after verification without another read.

The original manifest materialization reader used direct `SELECT payload`, bypassing `get`. Therefore merely changing the presence probe would not have been a safe lazy-validation policy. Both `get` and that reader now use `read_verified_payload`; corrupt payloads cannot enter a newly joined derived graph. Existing immutable rows are still not repaired or overwritten. A rejected row can prevent publication until the existing quarantine/new-key recovery policy resolves it.

The existing `views_corrupt_cached_payload_is_not_reused` test now asserts rejection at consumption, rather than expecting pointless re-extraction of an immutable corrupt row. The cached-content extraction test remains unchanged. Materialization tests now seed the real digest/schema columns rather than a payload-only mock table, and a new test rejects corrupt digest and schema at consumption. Already-published derived generations are not rescanned on a no-op publication.

### Key derivation and opening deployed stores

The numeric `payload_schema` column is **not directly hashed into FullKey**. Producer versions are:

- `blob_store::SemanticKey::full_key` hashes the domain `aft/blob-store/semantic/v1` and the source digest, relative path, chunker version, embedding-template version, and model fingerprint. `SemanticKey::for_current` supplies `SEMANTIC_PRODUCER_VERSION` for both producer components.
- `blob_store::CallgraphKey::full_key` hashes the domain `aft/blob-store/callgraph/v1` and the source digest, language, and extractor version. `CallgraphKey::for_current` supplies `CALLGRAPH_PRODUCER_VERSION`; publication instead supplies `views::callgraph::PRODUCER` (`ruled-callgraph-v3`) to `from_bytes`.
- The private `blob_store::full_key` helper length-prefixes every field and BLAKE3-hashes the domain and fields. The contract above the payload-schema constants, defended by `payload_schema_and_producer_version_pairs_are_pinned`, requires a producer-key version bump with each payload-format change.

Thus properly versioned producers cannot share a key across incompatible formats. This is a producer contract, not a SQLite constraint: a broken writer, an omitted version bump, or corrupted metadata can still create a same-key row with a bad schema field. Key-only membership deliberately makes no stronger claim; `get` and the materialization reader reject that row at consumption, and immutable `put` would not repair it by re-extracting anyway.

`opening_populated_pre_presence_store_adds_no_payload_index` independently constructs the deployed pre-change schema with 64 payloads sized 4–100 KiB, then calls the real `BlobStore::open`. It asserts that the populated table's index definitions and SQLite schema version are unchanged. The fixture does not copy `BLOB_SCHEMA`, so adding a new index to the opener cannot silently alter the fixture to match. Before the correction it failed because opening created `blob_presence`; restoring that DDL as a mutation also fails this test alone. This is a no-new-index-DDL regression, not a claim that SQLite performs zero metadata page reads.

No 1 GiB index-build benchmark is needed for this design: there is no new index or side table to build, and schema does not need a presence-time probe. No live stores were opened or modified for the investigation. An unused `blob_presence` left by an experimental earlier build is neither required nor rebuilt; no cleanup scan is added to the open path.

## Counted measurement

`views_shared_checkout_presence_reads_no_payloads` seeds 2,048 distinct real TypeScript callgraph blobs (9,592,960 payload bytes), then prepares and publishes a new checkout view over unchanged sources. Thread-local counters record bytes returned by the payload SELECT and payload-validation BLAKE3 calls; they exclude source/key hashing and unrelated test threads.

| Boundary | Payload bytes read | Payload-validation BLAKE3 calls |
|---|---:|---:|
| Before fix, candidate/presence phase | 9,592,960 | 2,048 |
| After fix, candidate/presence phase | 0 | 0 |
| After fix, actual cold materialization of the new view | 19,185,920 | 4,096 |
| After fix, repeat publication of the established unchanged view | 0 | 0 |

The cold materializer has two payload-consuming passes; these are real reads, now verified, not existence checks. Thus **zero payload reads applies to membership and to the subsequent unchanged no-op, not to building a new derived graph**. The baseline test went red with the exact nonzero candidate-phase counts above. The regression also requires successful publication with zero blob inserts. Additional tests check a large 8 MiB blob, the existing key index's covering SQL plan, lazy schema rejection, plane mismatches, quota reuse, and a read-only publication with a concurrent late fill.

## Rollback-journal attribution

The long-lived store with an unconfigured journal mode was:

`<storage>/views/<scope>/derived.sqlite` — `path_status::PathStatusStore`.

It is the annotation database, **not** a generation's derived callgraph database. `PathStatusStore::open` → `open_at` created schema without setting journal mode, leaving SQLite's default DELETE mode. `prepare_checkout` opens it on each attempt, calls `mark_pending` once per blocking/semantic-pending path, then `clear` once per complete candidate when there are no blocking paths. `upsert` and `clear` are individual autocommit statements. Repeated pending annotations therefore create/unlink a rollback journal per updating path even when `blob_puts=0`. Refresh and maintenance annotations use the same long-lived database.

`open_at` now migrates existing DELETE-mode stores to WAL with a busy budget and a contention retry. Synchronous durability is not weakened. The regression opens an existing DELETE-mode file, performs 32 annotations, checks WAL/no rollback-journal sidecar, and reopens to verify persistence. Before the fix it failed with `journal_mode = delete`.

Other audited paths at this revision:

- `aft.db`: `db::configure_connection` sets WAL.
- Original blob stores: `blob_store::configure_connection` sets WAL; v2 `members.sqlite` does so in `ensure_schema`.
- OID aliases and view pointers: their connection configuration sets WAL.
- Callgraph generations and staging builds: `configure_connection` and `configure_build_connection` both already set WAL. `prepare_for_atomic_swap` deliberately switches a completed build to DELETE only after checkpointing and checking sole connection ownership; this is the rename/publication boundary, not a repeatedly written long-lived DELETE store.
- Inspect project cache: `inspect::cache::configure_connection` sets WAL; inspect scope cache is in-memory.
- Symbol cache persistence uses binary `symbols.bin`, not SQLite.
- The older `alias::ManifestSqliteStore` leaves DELETE mode, but the production source has no construction call site; its tests/build snapshots are not the per-path annotation loop.
- Per-generation view materialization and its keeper use WAL.

The supplied syscall excerpt did not include journal filenames. The code audit and red/green test identify a concrete repeated DELETE-mode writer consistent with the unlink burst; they do not uniquely assign all 3,000 observed unlinks to it without a filename-bearing trace. No unrelated journal modes are changed.

## Why pending publications repeat

The profile outcome `pending` is set only when `blocking_paths` is nonempty. Those are callgraph blobs unavailable to a read-only publisher (`allow_blob_put=false`) or unsupported Git modes. Semantic-only pending paths can coexist, but do not by themselves cause this early-return outcome. In particular, zero inserted blobs does **not** mean every required blob exists: a read-only publication inserts none even when keys are missing. Producer/language/content differences also mean blobs stored under other keys do not satisfy the current key.

The blocking return happens before a candidate manifest or derived graph is published. The report contains the previous generation/manifest (or neither for a new view) and pending paths, not the newly assembled successful candidates. Commit installs this pending snapshot in the runtime. The detached scheduler treats it as a successful attempt and stops; it does not spin on pending. Subsequent semantic-ready/refresh triggers, watcher activity or configure work schedule another publication. Error retries are different: the detached loop retains its original full path union, because no update was installed.

Every attempt enumerates HEAD and checks checkout membership metadata. Empty `changed_paths` requests full source reads. A nonempty set can reuse entries from an existing published manifest, so ordinary semantic-ready retries already read only pending source paths (while still walking membership metadata). But a first blocking publication has **no published manifest to reuse**. `views_pending_first_publication_has_no_manifest_to_reuse` demonstrates this with eight unchanged files and one missing blob: first attempt reports one pending path/zero puts/no manifest; after another writer fills it, a retry naming only that path still reads all eight sources and publishes all eight entries.

No unsafe pending-only shortcut is added. Recovering only pending paths in that case needs a retained, pinned candidate manifest keyed to HEAD/source identity, invalidated on watcher/configure changes and cancellation; path annotations alone cannot reconstruct the other entries. With an older published base, narrowing an unsuccessful full attempt to just its blocking paths could also lose successful but unpublished edits. Skipping the metadata walk would additionally lose unreported checkout deletions. This is a separate candidate-retention change; the expensive repeated payload reads are removed without changing publication membership or cancellation semantics.
