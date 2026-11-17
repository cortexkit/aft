# Rollback safety of AFT on-disk state

Investigation date: 2026-09-28. This is an investigation and proposal, not a format change. The proposal was implemented on 2026-09-30; see "Implemented (2026-09-30)" at the end of the proposal section.

## Findings

**AFT does not yet implement the three-part placement rule:** readers must refuse newer formats by name, writers must durably stamp the minimum reader build before publishing a new format, and placement must reject a build below the highest floor on disk. Some low-level readers detect an unsupported version, but their callers treat it as a cache miss, a rebuild request, or invalid task state. The main SQLite migration gate is substantially safer than the cache gates, but even its named downgrade refusal does not stop the bridge: configure falls back to JSON-only persistence.

There are two different questions here:

1. **Does this particular release pair lose state?** The tested revisions share the important format numbers. In the ordinary cross-version runs, no pre-existing file was deleted; checkpoints and undo history remained available, and both directions completed successfully. Search, symbol, inspect, owner, and bookkeeping files were rewritten. That is not a test of a future-format barrier.
2. **Would the old reader preserve a genuinely newer format?** Synthetic future headers, applied to copies of main-created data, demonstrate that it would not: search, semantic, symbol, and owner files were replaced with old-reader versions; backup metadata was downgraded on the next write; a callgraph pointer was republished away from the unsupported generation; and a future bash task was moved to the invalid-task quarantine. These are not hypothetical consequences inferred solely from a `None` return.

The experiments did **not** demonstrate loss of user source files or a downgrade of `aft.db`. Cache loss still matters: it destroys expensive newer artifacts and can change available analysis. Quarantining task state and rewriting recovery metadata are more serious than losing a rebuildable cache.

## Revisions, builds, and evidence conventions

“Main” means the supplied main snapshot, **`6a21cb509f4cf4aa640a3581315f5d6463f5ce8d`**, not a moving remote ref. The old revision is tag **`v0.57.2`**, **`419e597ff2ef571588dcc6f7b22d616c7b3ec3e3`**. Both manifests identify the Rust package as 0.57.2, so a package version string cannot distinguish these builds.

Build environment: macOS, Rust/Cargo 1.98.1, debug profile, default features. Builds were sequential:

```sh
cargo build --locked -p agent-file-tools --bin aft
# Preserve target/debug/aft as the main binary before building the tag.
git archive v0.57.2 | tar -x -C target/rollback-audit/old
CARGO_TARGET_DIR="$PWD/target" cargo build --locked \
  --manifest-path target/rollback-audit/old/Cargo.toml \
  -p agent-file-tools --bin aft
```

Both builds succeeded. SHA-256 of the preserved executables:

| Executable | SHA-256 |
|---|---|
| Main | `ad1f38f014e84e13a13629224efe4beb084f730043ff3b0922e99bf6959e1130` |
| v0.57.2 | `ac35205c1509f2190c43466399ca30b613113d35fcd03685ccd1e1c7f4f1f477` |

Source references below are relative to `crates/aft/src/` at the main SHA unless prefixed **tag:**. Observed means execution of the preserved binary, not execution of a reimplementation of its reader. “Future probe” means an intentionally unsupported version **99** in a copied fixture; it is not a claim that main writes version 99. Unsupported-header probes do not synthesize a complete future layout.

Raw local evidence is retained under the task worktree's ignored `target/rollback-audit/`: `probe.py`, `future.py`, `followup.py`, `view-future.py`, per-run `.stdout`, `.stderr`, `.files.json`, and future-probe `.before.json`. Only this document is committed. The worktree is `bg_5296aecce78141f9` under the Alfonso worktrees directory. The tables and log excerpts here are the durable evidence summary; local scratch files are not a shipped test suite.

## Format inventory

The table distinguishes the **reader's decision** from what a later writer or cleanup path does. A version number in a struct is not itself an enforced floor. Read-only/worktree borrowers sometimes avoid the write that an owning bridge performs; that is an ownership restriction, not rollback protection.

| Persisted format / location | Version and evidence | Newer-version behavior and destruction risk | Observed v0.57.2 behavior |
|---|---|---|---|
| **`aft.db`**, including `-wal`/`-shm` | `schema_version(version)`, `MAX(version)`, current **13 in both revisions**. `db/mod.rs:31,396-407,438-470,472-519`; tag same constant at line 31. Not `PRAGMA user_version`. | `DowngradeRefused` names found/supported versions. Forward migrations run under `BEGIN IMMEDIATE`, re-read the version under the write lock, and update the version in the same transaction. No downward migrations. However PRAGMAs/schema-table initialization precede the check, and configure logs the error then runs without DB persistence (`commands/configure.rs:5499-5516`). This is not a process-level refusal. | Real pair: schema stayed 13; DB WAL/SHM changed as normal bookkeeping ran. Future 99: DB file SHA unchanged, version remained 99, configure still returned success with the explicit JSON-only fallback warning below. |
| **Tables inside `aft.db`**: bash task registry and metadata JSON, pattern watches, backups/restore metadata, compression events/rollups, harness/host state, standing roots/freshness, write ledger | Migrations 1–13: `db/mod.rs:33-335`. Watches v5/v9, roots v7, ledger v11/v12, task retention index v13. Backup `restore_meta` has its own JSON `version=1` (`backup.rs:49-50,3338-3367`). | The DB gate protects SQL schema evolution only when the global version is bumped. Unknown JSON/enum record variants within schema 13 are not automatically protected. Backup restore metadata with an unequal version returns `None`, not a named refusal. Regular retention and watch/backup mutation still operate when the enclosing DB version is accepted. | Bash rows and backup history were populated and read both ways. Not every watch, root, compression, or ledger record variant was separately synthesized. No SQL migration occurred between these two version-13 binaries. |
| **GitHub cache and alert tables in `aft.db`** | `db/github_read_cache.rs:10,84-90` explicitly creates its cache schema separately from historical migrations; `alert_records.rs:37-76,108-130` similarly calls `CREATE TABLE IF NOT EXISTS`. No independent schema version in those initializers. | Merely being in `aft.db` does not guarantee these table changes increment version 13. Unknown shape can result in SQL/decoding errors; additive compatible columns can be ignored. No independent future-record refusal. | Covered by opening `aft.db`, not by authenticated GitHub requests or injected alert variants. This is a coverage limit, not a passing row-compatibility test. |
| **Search/trigram `index/<key>/cache.bin`** (also view trigram artifacts and temporary spill/count files) | Outer `CACHE_MAGIC`, u32 `INDEX_VERSION=4` at byte 4; embedded postings/index header repeats version 4. `search_index.rs:30-55,1571-1593,1740-1761`. Spill/count magic strings are `AFTSPI01` / `AFTFTC01`. Same main/tag index version. | Any unequal version returns `None`, indistinguishable from a missing/unusable cache. Owning cold build/save can replace the path. No named unsupported-version error reaches the request. | Real pair: `cache.bin` rewritten both directions. Future outer version 99: old binary succeeded and replaced it with version 4 (`5849443104000000…`). |
| **Semantic `semantic/<key>/semantic.bin`** | Byte-zero base version **7**; reader accepts **6/7**. Appended `AFTSEG01` frames have their own version **1**, length and BLAKE3 checksum. `semantic_index.rs:594-617,6492,6953-6957,7160-7244`. Same main/tag. | Unknown base version logs incompatibility and returns `None` *without immediate deletion*. That is not preservation: the next persistence call replaces an unreadable base (`6629-6758`). A valid base followed by an unsupported committed segment can instead take the generic “corrupt semantic index” removal path. A torn tail can be truncated by an owner. | Real pair: 5,415-byte semantic snapshot was byte-identical after cross-open both ways. Future base 99: logged “rebuilding without deleting the shared artifact,” then “replacing base snapshot”; final first byte was 7. Segment-future deletion path is source evidence, not a separately executed segment probe. |
| **Symbol `symbols/<key>/symbols.bin`** | Magic `AFTSYM1\0`, format **3**, extraction schema **3** in the tested fixtures. `symbol_cache_disk.rs:17-24,129-145,202-224,323-325`. | Low-level reader produces an unsupported-format/schema message, but wrapper labels every failure **“corrupt symbol cache … rebuilding”** and returns `None`. Writer atomically replaces `symbols.bin`. | Real pair: rewritten. Future format 99: “corrupt symbol cache … unsupported symbol cache format version: 99 (expected 3)”; persisted again with format/schema 3/3. |
| **Callgraph generations**: `callgraph/<key>/<key>.g….sqlite`, legacy `<key>.sqlite`, `<key>.current` | SQL `meta.schema_version=1`, plus hashed content/schema fingerprint and `ready=1`. `callgraph_store/mod.rs:40,8467-8475,8641-8680`. `.current` is an unversioned filename pointer (`9252-9261`). Migrated generations have a separate JSON manifest `version=1` (`8139-8164`). | `database_ready` requires exact equality; `resolve_ready_target` reduces mismatch to absence/not-ready (`9273-9302`). Caller can cold-build and republish instead of refusing. Initializer writes its own schema/fingerprint with `INSERT OR REPLACE`; generation retirement has no future-format immunity. Migration manifest mismatch also reduces to false, and incomplete-migration cleanup must not be mistaken for compatibility enforcement. | Real pair: generation and pointer byte-identical on cross-open. Future `meta.schema_version=99`: warm inspect first bypassed the rebuild; after removing only the copied inspect cache to force cold analysis, a new schema-1 generation was published. The old schema-99 generation remained as previous; eventual GC loss is a code-path risk, not an observed immediate deletion. |
| **Inspect `inspect/<scope>/…sqlite` and `.current`** | SQL tables have **no database version** (`inspect/cache.rs:1987-2018`). `TIER2_CONTRIBUTION_CACHE_VERSION=34` in both revisions is folded into the aggregate hash (`185,2052-2087`), not stored as an independently readable version gate. Pointer is an unversioned filename. | Opens/create-if-missing tables; contribution blobs decode as generic JSON (`1168-1210`). Aggregate hash disagreement is a miss/recompute, not a named newer-format error. Writers update current rows; unsupported future SQL shapes may error. Old writer can replace newer contributions/aggregates without identifying their producer. | Existing inspect generation rewritten in both directions. No fabricated “future inspect version” probe: there is no such readable field to bump. |
| **View manifest JSON** `views/<scope>/manifest-<generation>.json` | `path_identity_version=1`, checked by custom deserializer; `views/mod.rs:437-485,703-710`. This versions path identity, not every semantic field in a manifest entry. | Unequal identity version returns **“unsupported path_identity_version N”**. Configure's view load can log failure and continue via rebuilding/fallback rather than refuse the bridge. Unknown entry shapes can be serde errors. | Committed-fixture manifests existed and cross-opened both ways. Future 99 logged a named view-manifest error; manifest and pointer remained in place during the bounded run, while search returned lexical fallback with `status=building`. |
| **View pointer and derived projection** `pointer.sqlite`, `derived-*.sqlite`/`derived.sqlite`, `trigram-*.bin` | Pointer table has no schema version (`views/mod.rs:868-889`). Derived graph uses callgraph meta plus `view_materialization_version` and manifest fingerprint (`views/materialization.rs:237-275`). Trigram uses search serialization. | Pointer initialization has no future gate. Materialization-version inequality discards the reusable base (`base=None`) and rebuilds projection rather than naming a newer producer. Independently version each of these; the JSON path version is insufficient. | Both directions opened populated committed views. No view-generation file disappeared; existing view files were byte-identical in the controlled cross-open comparison. Future derived-version probe not run. |
| **Content blobs** `blobs/<key>/{semantic,callgraph}.sqlite` | Per-row `payload_schema=1`, producer strings `semantic-v1` / `callgraph-v1` participate in content keys. No database-level schema version; `blob_store/mod.rs:79-101,706-725`. | `get` checks digest/schema; schema mismatch is a warning and **`Ok(None)`**, intentionally leaving the committed row (`577-620`). New producer keys reduce collision risk but do not give a named refusal. Corrupt database handling can rename the DB and sidecars to `.corrupt-<timestamp>` (`782-803`); do not conflate this with demonstrated future-schema handling. | Each committed fixture populated one semantic and one callgraph payload. Stores unchanged across cross-open. A schema-99 callgraph row remained, but the derived view served without a blob mismatch log: the future row-read branch was **not demonstrated reached**. Source establishes the miss behavior. |
| **OID aliases and alternate SQLite manifest representation** | `blobs/<key>/oid-alias.sqlite` has no version (`alias/mod.rs:43-59,313-332`). `ManifestSqliteStore` has `manifest_metadata.path_identity_version=1`, but `open` only creates tables (`637-647`). | Alias rows are typed hashes, not versioned records. Alternate manifest `write` validates the incoming manifest version, deletes existing entries, then replaces stored metadata (`650-675`); it does not first reject a newer stored version. `paths()` does not check the stored version (`678-695`). | Alias SQLite file created and reopened both ways. Alternate `ManifestSqliteStore` was source-audited, not produced by the bridge fixture. |
| **Bash task files** `<harness>/bash-tasks/<session-hash>/<task>/control/metadata.json`, I/O and control siblings; legacy flat layout | `schema_version=6`; reader accepts 2–6 (`bash_background/persistence.rs:29,484-531,1181-1238`). Raw stdout/stderr/PTY, exit marker, command/wrapper, environment bytes and `manifest.blake3` are separate payloads/integrity metadata, not a minimum-reader envelope. | JSON reader names unsupported schema, but replay catches layout/read failures and **quarantines** non-live tasks as invalid (`bash_background/registry.rs:2934-2999`). Writer stamps 6 after successful parse. Live-task protection is conditional; it is not a general future-format guard. | Real completed task metadata/output retained and completion/history available both ways. Future schema 99: whole directory moved to `bash-tasks-quarantine/…invalid-…`; `bash_status` still returned a completed DB fallback record with empty output preview. Bytes survived in quarantine, but the normal task path no longer existed. |
| **Backup stacks** `<harness>/backups/<session-hash>/<path-hash>/meta.json`, immutable `.bak` content, session marker | JSON `schema_version=4`; layout `format_version="v2"`; DB restore metadata version 1. `backup.rs:49-50,93-97,2027,2218-2253,2769-2796`. | The normal metadata reader does not validate `schema_version`: it parses generic JSON, path and count. The constant's comment promises reader validation, but the inspected read path does not enforce it. Legacy upgrade unconditionally inserts schema 4. Unknown layout is not equivalent to a named future refusal. Subsequent stack publication can overwrite metadata and retention can remove old content. | Real undo history/content survived both ways. Future schema 99 was accepted by `edit_history`, remained 99 on read, then became 4 after a normal pre-write backup. This proves metadata downgrade, not loss of the synthetic fixture's known entries. |
| **Durable checkpoints** `<harness>/checkpoints/<session-hash>/<name>/meta.json` and blobs | `schema_version=1`; typed metadata, check at `checkpoint.rs:1076-1100` (tag `1051-1075`). List hydrates before returning (`621-646`). | Named “unsupported durable checkpoint metadata schema N” propagates as an I/O error; does not automatically rebuild it. Reader deserializes full metadata before checking version, so an incompatible shape could instead fail parsing first. No per-store build floor. | Real checkpoint listed both ways. Future 99: list returned `io_error` with that exact message and path; metadata SHA unchanged, schema still 99. |
| **Artifact ownership** `artifact-owners/<key>/owner.json` | JSON `schema_version=1` (`artifact_owner.rs:14-25`). | `read_manifest` only deserializes (`584-595`), does not compare schema. Same checkout/family claims rewrite it (`120-143`); malformed manifests are removed (`187-189`). Future parseable ownership can be reinterpreted; incompatible shape can be deleted as malformed. | Real owner record rewritten for each process. Future schema 99 was silently replaced by schema 1 on configure. |
| **Build-death breaker** `build-breaker.sqlite` (observed under callgraph key), not just `aft.db` | No SQL format version; `configuration_version="v1"` is a breaker policy version. `build_breaker.rs:23,164-204,477-497`. | `CREATE TABLE IF NOT EXISTS`; policy-version inequality resets death counters and suspension fields in `ensure_record`, regardless of which version is newer. This can erase safety history on rollback. Unknown persisted domains decode to `None` (`65-72`). | File created by both binaries, unchanged during ordinary cross-open. Future policy reset is source evidence, not an injected admission test. |
| **Write ledger / census** | Durable ledger tables are in `aft.db` v11/v12, not a separate versioned ledger file. `write_ledger.rs:487-594` folds counters into tables and prunes seven-day history. `lifecycle_census.rs:69-100` is an in-memory snapshot/cache. | DB schema gate applies to tables, not future domain semantics. Census command/telemetry JSON outputs are reports, not replayed authoritative state; no reader floor was found for those outputs. Do not add an imaginary census-file migration to placement. | Short probes did not wait for a minute fold or separately exercise report importers. DB opened; no claim that every ledger row variant was tested. |
| **Config JSONC**, user/project tiers | No enforced format version or minimum reader; `$schema` is an optional string (`config_resolve.rs:99-140`). Main translated feature keys differ from the tag's raw `search_index`/`semantic_search`/`callgraph_store` (`tag:config_resolve.rs:100-139`). | Strict deserialization is followed by **partial per-key parsing**, so unknown/invalid top-level keys can be omitted (`config_resolve.rs:859-897`). Nested unknown keys are generally ignored. Main has explicit removed-key rejection/translation, not a universal future-config refusal. Doctor/config writers need the same floor before rewriting documents. | A preliminary tag run with no explicit enables produced no persistent search/semantic/symbol index (tag defaults are false, `tag:config.rs:690-694`). Final runs used revision-appropriate config, not falsely identical config. Unknown future config behavior is source-audited; no claim that successful store migration proves unchanged feature configuration. |

### Other persisted families and boundaries

These also need consideration when assigning a floor; they are not protected merely because some other store uses `schema_version`.

| Family | Current policy / source evidence | Execution coverage |
|---|---|---|
| `effective-path.json` | `schema` plus shell/startup-file fingerprints; mismatch makes cache invalid and eligible for replacement, not a named future refusal (`effective_path.rs:563-616`). | Created/rewritten on cross-open. |
| `cache-keys.json` | Unversioned map of root to key/root-commit/timestamp. `search_index.rs:56,114-118,5976-6005`; maintenance's read-only parse error leaves artifacts alone. This is not a future-schema contract for normal memo writes. | Created by the committed-Git fixtures. |
| Writer leases, index/symbol locks, reader markers | Lock JSON has no schema version (`fs_lock.rs:153-172`); reader marker is pid/hostname/time (`root_cache.rs:657-689`). Unknown shape can fail parsing/liveness checks. A floor must protect ownership and reclamation, not just payload reads. | Runtime locks/markers used; most vanish on graceful shutdown. No incompatible-live-lease probe. |
| URL cache content and `<hash>.meta.json` | No explicit format version; cached URL/content type/extension/time. Cleanup removes unparseable metadata and expires content (`url_fetch.rs:166-225,247-249`). | Not fetched in this audit; source-only. |
| Semantic migration disposition | View `migration-state.json` is an unversioned disposition/reason marker and existence alone means rebuild already scheduled (`migration/mod.rs:556-608`). | Not created by clean-storage probes; source-only. |
| Model/runtime downloads, generated executable shims, logs | Model/runtime artifacts carry upstream formats and loader compatibility, not a common AFT storage floor (`local_embed.rs:37`, `semantic_index.rs:3462`). Logs and generated `shims/gh` are not authoritative application-record readers. Ordinary log retention is not evidence of index deletion. | `shims/gh` changed to the selected executable; logs were added. No model-upgrade/downgrade campaign. |
| Authentication/runtime transport state | Per-command gh tickets are explicitly memory-only and die on restart (`gh_shim_ticket.rs:11-14`). Supervisor connection/credential files are not generated by the standalone bridge fixture; their separate protocol/owner must be included in a deployment-wide inventory. | No ticket secrets or supervisor credentials copied into evidence. |

This inventory covers the AFT-owned store families above, including formats not enabled by default. It is **not** a claim that every external LSP's cache, supervisor-owned connection format, telemetry consumer, platform download, or possible future record variant was dynamically exercised.

## Experiments and observed transitions

### Harness

The executable is an NDJSON bridge, not a conventional `--help` CLI. Each process used a fresh or deliberately reused `AFT_STORAGE_DIR` inside `target/rollback-audit`, `XDG_CONFIG_HOME` pointed at scratch, harness `runner`, and session `rollback-audit`. Sources were a tiny Python Git fixture:

```python
def greet(name):
    return 'hello ' + name

def main():
    return greet('world')
```

The baseline fixture had an initialized Git directory; the view fixture had a committed `sample.py`, which is necessary to populate the content-addressed manifest rather than merely create empty stores. Producer and consumer always used the **same project path**. Two separate storage roots tested main→old and old→main. Only the producer issued write/checkpoint/bash operations; consumers listed checkpoints/history instead of overwriting them as part of the test.

For reproduction, launch the preserved binary with pipes, send one request, and wait for its matching `id` response before the next; retain asynchronous frames too. Most arguments are flattened; `bash`'s shell command is nested to avoid colliding with the envelope command. Representative requests:

```json
{"id":"configure","command":"configure","session_id":"rollback-audit","project_root":"/absolute/fixture","harness":"runner","aft_search_registered":true,"config":[{"tier":"user","source":"audit-wire","doc":"{\"indexes\":{\"search\":true,\"semantic\":true,\"callgraph\":true},\"views\":{\"enabled\":false}}"}]}
{"id":"outline","command":"outline","session_id":"rollback-audit","file":"/absolute/fixture/sample.py"}
{"id":"search","command":"semantic_search","session_id":"rollback-audit","query":"greet"}
{"id":"inspect","command":"inspect","session_id":"rollback-audit","sections":["dead_code","unused_exports","duplicates"]}
{"id":"checkpoint","command":"checkpoint","session_id":"rollback-audit","name":"audit","files":["/absolute/fixture/sample.py"]}
{"id":"write","command":"write","session_id":"rollback-audit","file":"/absolute/fixture/scratch.txt","content":"audit\n"}
{"id":"bash","command":"bash","session_id":"rollback-audit","params":{"command":"printf rollback-audit","workdir":"/absolute/fixture","wait":true}}
```

For tag configure, replace the `indexes` object with top-level `search_index`, `semantic_search`, `callgraph_store`, all true. For view runs set `views.enabled=true`. Consumers use `list_checkpoints` and `edit_history(file=…/scratch.txt)` after analysis. Allow three seconds after configure and after the request sequence, close stdin, and wait for graceful process exit before hashing every regular storage file (relative path, size, SHA-256). Startup requests returning success are insufficient: verify expected artifacts exist and index requests report readiness.

An initial harness attempt used wrong flattened/nested argument shapes; those error responses were discarded, not counted as compatibility passes. Another preliminary run omitted the tag's index enables; it was replaced with explicit config before making the index claims. The final evidence labels are `main-create`, `old-open-main`, `old-create`, `main-open-old` for `main-enabled`/`old-enabled`, and `vc-*` for committed views.

These were local standalone processes, not a deployed SUBC placement test. `AFT_STORAGE_DIR` isolates the audited stores but does **not** isolate every process-global behavior: login-shell probing and the transient search-cache sweeper also ran. Their logs are not counted as changes inside the experimental storage root. No rollback binary was placed into the live deployment.

### Real main ↔ v0.57.2 results

All final configure, outline, search, inspect, checkpoint/write producer calls, and checkpoint/history consumer calls succeeded; each process exited 0. Search returned the `greet` symbol. The completed bash command produced `rollback-audit` and exit code 0; consumers received completion state.

| Artifact group | Main data opened by tag | Tag data opened by main |
|---|---|---|
| Pre-existing storage files deleted | **0** | **0** |
| `aft.db` | Schema 13 retained; WAL/SHM changed | Schema 13 retained; WAL/SHM changed |
| Search and symbols | Rewritten, no unsupported-version warning | Rewritten, no unsupported-version warning |
| Semantic | 5,415-byte file, same SHA | 5,415-byte file, same SHA |
| Callgraph generation / `.current` / breaker | Same SHA; WAL/SHM could be created on open | Same SHA; WAL/SHM could be created on open |
| Inspect | Existing generation changed | Existing generation changed |
| Backups/checkpoints/task metadata and raw output | Retained; history/checkpoint available | Retained; history/checkpoint available |
| Owner/effective PATH/shim/logs | Owner/PATH/shim rewritten; new log | Owner/PATH/shim rewritten; new log |
| Committed views/blob/alias files | Present and retained; no generation replacement | Present and retained; no generation replacement |

Search/symbol rewrites here are not proof of corruption: both readers support the same versions, and the producer's later scratch-file write creates normal freshness work. Similarly, a WAL/SHM change alone is not a logical table migration. **Forward open was successful; “forward migration is fine” cannot be generalized to an untested schema transition, because both binaries write schema 13.**

### Unsupported future versions: binary observations

Each probe copied a main-produced storage tree after process shutdown, changed one targeted field, recorded the before hash inventory, and opened it using the unmodified tag executable. Source code was not mutated. For binary headers, only the leading version field was changed: search u32 LE at offset 4, semantic u8 at offset 0, symbol u32 LE at offset 8. JSON probes changed only `schema_version`, except view manifests changed `path_identity_version`. SQLite probes used transactions to change the relevant metadata row.

| Probe | Observed result |
|---|---|
| Search 99 | Successful search; target SHA changed; header became version **4**. No named version refusal. |
| Semantic 99 | Target SHA changed; final byte-zero version **7**. Logs below demonstrate both the nondeleting read and replacing write. |
| Symbol 99 | Target SHA changed; format/schema **3/3**. Unsupported data called “corrupt.” |
| Callgraph meta 99 | Warm cached inspect did not reach cold rebuild. Removing only the copied inspect cache then running inspect changed `.current` from `…g1790600431432756000.15349.sqlite` to `…g1790600870215338000.71714.sqlite`. Previous file still existed with schema 99. |
| Owner 99 | Target SHA changed; final `schema_version=1`. |
| Backup 99 | History read succeeded, initially preserved schema 99. Subsequent ordinary write returned `backup-1`; `meta.json` became schema **4**. |
| Checkpoint 99 | `list_checkpoints` failed with `io_error`, naming unsupported schema 99; SHA unchanged. |
| Bash metadata 99 | Non-live completed task moved out of the normal task tree into invalid-task quarantine; the metadata/output files survived there. DB fallback masked this in `bash_status`, which still succeeded. |
| `aft.db` 99 | DB SHA unchanged and version still 99. Named warning, but configure succeeded and bridge used JSON-only persistence. |
| View path identity 99 | Named load error; bounded run left manifest/pointer intact, semantic search reported rebuilding and served lexical fallback. |
| Blob payload schema 99 | Row left in place. No mismatch log: this warm derived-view path did not establish a call to `BlobStore::get` on the changed row. **Do not count this as a tested future-blob refusal.** |

Representative stderr, with only fixture path prefixes abbreviated:

```text
cached semantic index version 99 is not compatible with 7, rebuilding without deleting the shared artifact
semantic index delta baseline unavailable (unsupported on-disk semantic version: 99); replacing base snapshot
corrupt symbol cache at <storage>/symbols/<key>/symbols.bin: unsupported symbol cache format version: 99 (expected 3), rebuilding
quarantining unresolved background task bash-d50ac073ee63ac90: unsupported background task schema_version 99 (expected 2, 3, 4, 5, or 6)
failed to open aft.db at <storage>/aft.db: database schema version 99 is newer than supported version 13 — running with JSON-only persistence
content-addressed view load failed: view manifest JSON failed: unsupported path_identity_version 99
```

No synthetic future inspect/alias/SQL-table-shape/segment/breaker-policy tests were run. No conclusion about such a test is inferred from an unchanged hash of an artifact whose read path was bypassed.

## Where rollback can lose or invalidate newer data

Prioritize these separately from ordinary cache retention:

1. **Proven replacement:** search, semantic, symbol, owner metadata. A “rebuildable” artifact still needs to survive an unsupported reader under the proposed rule.
2. **Proven metadata downgrade:** backup `schema_version` 99→4 on the next write. Unknown future fields or meanings are not fenced; the probe retained known backup entries, so it does not establish actual user recovery-byte loss.
3. **Proven operational removal:** future completed bash task quarantined as invalid. Its files survive, but normal replay/status access no longer uses their original paths. Do not describe this as unlinking the payloads.
4. **Proven replacement of authority:** future callgraph generation bypassed and `.current` republished. The future generation survived the first rebuild; subsequent generation GC can retire it. No version-aware preservation promise exists.
5. **Source-established rebuild/reset risks:** inspect contributions/aggregates, derived views, alternate manifest writer, breaker history, malformed owner/URL metadata, and incompatible semantic segments. These are not all dynamically demonstrated destructive events.
6. **Partial safety, not placement safety:** `aft.db` and checkpoint readers name the incompatible version, but SQLite refusal degrades to JSON-only operation, and neither supplies a minimum reader build for placement. Blob schema mismatch is a miss, not a refusal; content-addressing alone does not satisfy the rule.

## Proposal: implement the three-part rule

### 1. Name the incompatible format before reading or changing its payload

Introduce a shared typed error such as `UnsupportedPersistedFormat { store, path, found, supported, minimum_reader_build }`, with a stable wire code such as `storage_requires_newer_reader`. It must survive propagation through configure, status, task replay, cache hydration, migration, and cleanup.

* Decode a small stable envelope/header first. Do not deserialize an entire future payload and call the resulting serde error corruption.
* Distinguish **missing**, **recognized older and migratable**, **recognized current**, **corrupt recognized format**, and **unsupported newer**. Do not collapse these to `Option`/false/cache miss.
* Unsupported-newer means no overwrite, truncation, quarantine, GC, fallback migration, or silent empty store. Diagnostic/read-only tooling may report it without opening a mutating connection. In particular, remove JSON-only fallback for the typed downgrade case, while retaining separately justified fallback for other errors.
* Add explicit store-level versions to inspect, pointer/derived databases, alias DB, breaker DB, cache-key memo, and coordination metadata. Keep payload/record versions separate from SQL structure and analysis-content fingerprints. A fingerprint that cannot be ordered is not a readable future-version fence.
* Apply record envelopes to JSON payloads inside accepted SQLite schemas too. Audit enum decoding/defaults: schema 13 cannot protect a new record kind introduced without bumping it.
* Preserve incompatible objects even in maintenance. “Old generation,” “orphan,” “malformed owner,” and “expired cache” must not bypass the unsupported-format refusal.

### 2. Stamp the minimum reader **before** publishing new-format state

Proposed storage-root file: `<AFT_STORAGE_DIR>/reader-floor.json`, with a stable bootstrap schema and per-store requirements, for example:

```json
{
  "floor_schema": 1,
  "minimum_reader_build": "<ordered compatibility build id>",
  "stores": {
    "aft.db": {"format": 14, "minimum_reader_build": "<build id>"},
    "semantic": {"format": 8, "minimum_reader_build": "<build id>"}
  }
}
```

The example numbers are proposals, not formats currently written. Use an explicitly ordered, release-registry-backed compatibility build identity (or ordered compatibility epoch plus immutable build identity). Do **not** order Git hashes, compare strings lexically, or trust the package version alone: this audit's two different executables both say 0.57.2.

Under a cross-process storage/placement coordination lock, a writer must:

1. Read and validate the existing floor and relevant store envelope without mutation.
2. Atomically raise the floor to the maximum of existing and required reader builds; fsync the temporary file, rename, then fsync the parent directory.
3. Only after durable floor publication, publish new-format files or commit the schema/record transition. A crash between steps 2 and 3 conservatively blocks rollback; the opposite order permits data destruction.

Never lower the floor automatically because a current artifact was deleted, a cache was rebuilt, or one harness is empty. Lowering requires explicit offline, verified conversion/removal of **all** higher-floor data. Keep per-artifact envelopes so the root file can be audited/reconstructed and copied stores cannot silently outrun their destination root. A missing/corrupt/unknown floor on a populated root is not evidence that any binary is safe; bootstrap with a nonmutating inventory and a known legacy compatibility policy.

Config may live outside `AFT_STORAGE_DIR`; a new config writer must stamp its owning floor too. Include harness partitions, standing roots, worktree-shared artifacts, alternate storage roots, durable tasks/checkpoints, migration leftovers, and generations no longer current. Retaining an old binary alone is not a complete rollback strategy; retain a compatible pre-upgrade data snapshot when lowering a floor is required.

### 3. Refuse incompatible placement, including rollback placement

`scripts/stage-card.sh:21-45,73-129` currently checks a committed source tree, builds/copies/signs an executable, verifies freshness/hash/debug symbols, and publishes `ck-aft.current`. It does **not** read an on-disk reader floor.

Add a pre-publication gate, before writing the current-card declaration, using a stable read-only compatibility checker. Proposed interface, not existing CLI:

```sh
storage-floor-check \
  --candidate "$STAGING/$CARD" \
  --storage-root "$RESOLVED_AFT_STORAGE_DIR" \
  --include-config-roots --require-known-floors
```

The checker must obtain candidate capabilities from signed/hashed build metadata or a guaranteed side-effect-free command. It must not start an old daemon against the data to ask whether it is compatible. Missing candidate capability metadata is refusal, not “old enough to predate floors.” Name the blocking store/path, observed format, floor, and candidate build in the diagnostic.

Staging is advisory for a remote destination. The **actual placer/supervisor must repeat** the comparison against the destination's highest floor immediately before replacement, under the same coordination protocol that excludes format writers. Otherwise a writer can raise a floor between stage-card and placement. Automatic rollback must use this exact gate too; it must not rename yesterday's image into place merely because today's process failed health checks. Include all configured/standing storage roots and config locations, not just the staging user's default home.

Roll out the stable floor reader/placement refusal **before** any writer starts raising a new format floor. It cannot retroactively make v0.57.2 respect a floor file; enforcement outside that binary is what makes retaining it as a rollback image safe.

### Acceptance evidence for a later implementation

* Cross-open real old/current fixtures in both directions and compare logical SQLite data as well as closed-file inventories; a WAL file difference is not a row-loss oracle.
* Probe each store's future envelope and each independently versioned embedded record. Require the named error and unchanged payload/pointer/floor, including replay and GC paths.
* Ensure the probe actually reaches the future reader: warm inspect and derived-view shortcuts in this audit concealed the callgraph/blob paths.
* Crash writers between floor fsync, payload publication, and migration commit. Verify the floor never lags the highest committed artifact requirement.
* Test stage→placement races, alternate storage roots, unknown floor schema, missing metadata, restore from a snapshot, and automatic rollback. Include a negative control that demonstrably refuses an incompatible candidate rather than merely exercising the current binary against its own files.

### Implemented (2026-09-30)

The three-part rule is now in AFT for the artifacts the fleet design (extensibility r7.3 §16.8) names for AFT (`aft.db`, the search, semantic and callgraph index formats) and for the other versioned stores this audit showed being overwritten, downgraded or quarantined: symbol cache, artifact ownership, undo backup metadata and background bash task metadata. The current formats are the baseline; no format was bumped.

**1. Named refusal.** `crates/aft/src/persisted_format.rs` defines one typed refusal, `UnsupportedPersistedFormat { store, path, found, supported, source }`, rendered as `storage_requires_newer_reader: <store> at <path> has format version <found>; this build reads up to <supported>`. Each store reads only its version header or field before anything else and refuses a version above the one it reads:

| Store (floor key) | Header read | Where refusal stops the old behaviour |
|---|---|---|
| `aft.db` | `MAX(version)` through a read-only, never-waiting connection | Before the read-write open, PRAGMAs and migrations; tools are refused with `database_unavailable` carrying the refusal (the JSON-only fallback this audit saw is already gone on main). `run_migrations` keeps its `DowngradeRefused` for opens the peek cannot make. |
| `search` | outer `cache.bin` header | Reader returns a miss; `artifact_write_allowed` refuses, so the cold build settles on the existing build-denied (empty) index and nothing is written; the orphan sweep skips it. |
| `semantic`, `semantic_segment` | base version byte; segment version while decoding | Reader returns a miss without deleting; the configure lane refuses before contacting the embedding backend; `write_to_disk` never appends, compacts or replaces. A file shorter than the version-1 header is still removed as corrupt, as before. |
| `callgraph` | `meta.schema_version` of the published generation (or legacy DB), read-only | Every opener, cold build, migration and `publish_pointer` refuses; `callgraph_store_for_ops` answers with the refusal instead of starting a build, so the pointer is never republished and no generation GC runs. |
| `symbols`, `symbols_extraction` | file format and extraction schema in the header | Reader refuses by name instead of calling it corrupt; the writer refuses under its lock. |
| `artifact_owner` | `schema_version` before the full shape | The manifest is never rewritten, reclaimed or removed as malformed; the root borrows read-only with the refusal as the owner note. |
| `backup_meta` | `schema_version` of `meta.json` and the session marker | History reads and the pre-write backup fail with the refusal; the stack metadata, marker and content stay as they are. |
| `bash_task` | `schema_version` before the full shape | Replay, relaxed lookup and GC can no longer quarantine it: both quarantine entry points refuse, so the task stays under its original path. |

Refusals are recorded in a process-wide registry keyed by path (artifact path, or the callgraph directory, or the storage root for floor refusals). Writers consult it, and the status surfaces read it: `observed_index_status` (and so every index consumer) reports the plane `unavailable` with the refusal as its reason; the `status` command shows the same for `search_index`/`semantic_index`, adds `storage_refusals` (store, path, found, supported, message) and a `storage_requires_newer_reader:<store>` entry in `degraded_reasons`. Configure also peeks this project's search, semantic, symbol, callgraph and owner headers before any loader or owner claim runs, so the refusal is known from the start rather than discovered halfway through a rebuild.

**2. Floor.** `crates/aft/src/reader_floor.rs` keeps `<storage root>/reader-floor.json`:

```json
{"floor_schema": 1, "stores": {"aft.db": 13, "search": 4, "semantic": 7, "semantic_segment": 1, "callgraph": 1, "symbols": 3, "symbols_extraction": 4, "artifact_owner": 1, "backup_meta": 5, "bash_task": 6}}
```

Deviation from the proposal: the floor records per-store *format versions*, not a `minimum_reader_build`. The proposal asked for an ordered, release-registry-backed build identity because package versions and Git hashes cannot be ordered; no such registry exists, while each store's format version is already an ordered integer owned by that store, and "can this build read format N of store S" is exactly the question placement asks. A build advertises its per-store maximum through `aft --formats`, so no build identity has to be ordered at all.

Raising takes the per-store maximum under a cross-process lock (`reader-floor.lock`), writes a temporary file, fsyncs it, renames it over the floor and fsyncs the directory; it never lowers a store and preserves store names it does not know. `publish_with_floor` raises before running a new-format write, so a crash in between leaves the floor ahead of the data. A floor with a newer `floor_schema`, or one that does not parse, is never rewritten, and every store is refused. Configure calls `reader_floor::prepare` for the storage root before anything under it is read or written: it records a floor refusal for each store whose floor is above this build and then writes today's written formats as the baseline, which is how the next format bump raises the floor before its first write. Startup choice: a build below the floor refuses the affected components by name rather than refusing to start, because the floor is per store and the stores map to components; file tools and any component whose store is still readable keep working, and each refused component reports the floor as its reason.

**3. Placement.** `scripts/stage-card.sh` sources `scripts/lib/storage-floor-check.sh` and, on the exact signed bytes it is about to stage and before `ck-aft.current` names them, runs the card's `aft --formats` (side-effect free: it returns before storage, logging or PATH setup) and compares it with the live root's floor. It refuses when a store is below the floor or unknown to the card, when the card cannot report its formats (every build older than this change), or when the floor cannot be read or has a newer `floor_schema`. A root with no floor yet passes with a note: no floor-aware build has opened it, so only formats every build reads are there. `--skip-build` cards take the same path, and the script runs standalone (`scripts/lib/storage-floor-check.sh <binary> [storage-root]`) to vet any image, a rollback image included. The storage root follows the product's plugin-less ladder; `CK_AFT_STORAGE_DIR` overrides it.

**Evidence.** `tests/integration/rollback_safety_test.rs` plants a version-99 artifact for search, semantic (with a working embedding endpoint, so an unguarded lane would really rebuild), callgraph, symbols, owner and `aft.db`, configures current code over it, and checks the named refusal, byte-identical files (plus no new callgraph generation and no pointer change) and the unavailable status; a further test puts the floor above this build and checks that nothing is written and the reason names the floor. Unit tests cover backup metadata and bash task replay/GC, and the floor (baseline, never lowered, raised before the write, a failed raise skips the write, a simulated crash before the rename leaves the old floor intact, above-floor and newer-schema refusals). `tests/integration/stage_card_floor_test.rs` drives the placement gate with this build's own binary against temporary roots: equal floor stages, floor above the card refuses and names the store, unknown store, newer floor schema and a card without `--formats` refuse, and stage-card runs the gate before staging.

**Not covered yet.** The unversioned or separately versioned families in the inventory above still lack envelopes and refusals: inspect caches, view manifests, pointers and derived projections, blob and alias stores, the build breaker, `cache-keys.json`, locks and leases, URL cache metadata, config JSONC, and `restore_meta` rows inside `aft.db` (an unknown version still falls back to disk metadata). Legacy harness-partition callgraph stores read during migration are not gated. Only configure runs the floor startup check, so a writer outside a configured session (standalone CLI subcommands, standing roots of a daemon that never configures that storage root) relies on the per-artifact refusals alone. Placement is enforced by `stage-card.sh`; the real placer and any automatic rollback must repeat the same comparison against the destination root immediately before replacing the binary, as the proposal says, and that code lives outside this repository.
