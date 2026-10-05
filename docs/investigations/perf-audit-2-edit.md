# Edit, hashline and durable-history performance review

Baseline: `54a3d93e230e23e580c40a0164d5cfe2bdc9561d`. The audit's locations are
older than this baseline. Locations below refer to the implementation reviewed
in this change. No on-disk format, tool argument, flush, fsync, transaction
commit point, or LSP manager/transport implementation was changed.

## Measured changes

Counts come from hooks at content reads, parser calls, vector-shift sites,
owned blob serialization, and normalization calls, not elapsed time. Each
counting assertion was observed red with the old implementation or with the
optimization disabled, and green with the live implementation. The additional
undo fixture measures both cold and warm stores. Its cold count is intentionally
unchanged: selective cold hydration would change unrelated-corruption failures.

| Audit finding | Current evidence / verdict | Before | After | Output evidence |
| --- | --- | --- | --- | --- |
| Full history content re-read for every snapshot (audit 293) | **Fixed**: `backup.rs:3825`, `entry_from_v2_meta`. Cache only blob bytes; still read and decode authoritative metadata, including post-state annotations. | 20 history content reads for a steady-depth append | 1 read of the previously uncached newest entry | 20-entry, 256 KiB binary stack; every byte and lossy text view checked; undo restores identical binary bytes |
| Operation undo hydrates every session stack (294), warm content reads | **Fixed** for repeated content reads; `backup.rs:3502`, `load_from_disk_if_needed_locked` | 640 reads for 32 files with 20 entries each, despite priming memory | 0 warm content reads | Latest operation still restores exactly one file with the same bytes and operation id |
| Operation undo hydrates every session stack (294), cold selection | **Deferred**: candidate discovery at `backup.rs:3552` and the locked hydration loop still cover all stacks | 640 cold content reads | 640 cold content reads | Preserves the existing fail-closed behavior when an unrelated stack has missing/corrupt content; selecting just the newest operation needs a separate error-contract decision |
| Re-read metadata after hydrating a backup stack (part of 294/304) | **Fixed**: `backup.rs:3502,3718`; carry the metadata already decoded under the stack lock into `disk_index` | 2 metadata reads/parses per hydration | 1 metadata read/parse | Same 20-entry history count; existing stale-stack, restart, missing-content and future-schema tests remain green |
| Every checkpoint operation re-reads every blob (295) | **Fixed**: `checkpoint.rs:950`, `read_checkpoint_from_disk`; metadata bytes and all blob versions guard reuse | 632 blob reads across warm list, file-path lookup, delete and restore, with 20 checkpoints × 8 files × 256 KiB | 0 blob reads across those operations | Listed names/counts/timestamps match; all eight restored binary files match; corrupt metadata still skips only that checkpoint |
| Consumer parsing without a prefilter (296) | **Fixed** for parsing: `commands/move_symbol.rs:1114,1788`; conservative import/export prefilter and retained per-thread grammar parser | 513 parses for 512 scripts plus one importing module | 1 parse | Literal expected consumer import rewrite; integration suite covers named/default/aliased imports, namespaces, re-exports, shadowing and source-local references |
| Per-line hashline splicing (298) | **Fixed**: `hashline/apply/edits.rs:219`; one baseline-order materialization, cloning only retained rows | 50,000,000 retained-tail row shifts for a 10,000-line middle cut from 20,000 lines | 0 row shifts | Independent expected CRLF bytes plus 2,560 mixed/boundary cases against a frozen splice oracle, including zero and past-EOF anchors |
| Single-edit repeated reads (299) | **Fixed** for the shared pipeline: `edit.rs:631,722` | 4 pipeline content reads on a 20,000-line TS file, excluding caller/backup/LSP reads | 2 pipeline content reads | Same syntax-valid result, formatter reason, unchanged reflow excerpt, output bytes, invalid-edit rollback and rollback bytes |
| Repeated parses / fresh FileParser (299) | **Already fixed** for grammar allocation in `parser.rs:1016,1588`; two parses remain necessary for pre-edit rollback eligibility and post-format validation | 2 parses | 2 parses | Shared pipeline now parses its captured buffers through the same retained parser helper; no claim of removing the two validity checks |
| Rename rescans and string shifting per LSP edit (300) | **Fixed**: `commands/lsp_rename.rs:437,486`; requested UTF-16 positions resolved once per line, then one output construction | 819,021,000 line-scan bytes for 1,000 edits near EOF in a 420,000-byte CRLF/Unicode file | 430,000 scan bytes (420,000 input bytes + 10,000 requested-line character bytes) | Independently constructed expected Unicode/CRLF output; 2,560 cases against the sequential oracle. Overlaps, invalid lines and oversized columns retain the sequential fallback |
| Bytewise scanner copying (part of 301) | **Fixed**: `hashline/scan/mod.rs:508`; append chunks and line segments in bulk | 2,162,688 individual vector pushes on the chunked fixture | 0 individual vector pushes | Entire snapshot compared with a frozen bytewise scanner; existing BOM, CRLF, ranged-retention and oracle corpus tests remain green |
| Normalize twice for a snapshot tag (part of 301) | **Fixed**: `hashline/scan/mod.rs:556`; hash the normalized bytes already retained by the snapshot | 2 normalization passes | 1 normalization pass | Tag agrees with the unchanged oracle's `tag_for`; pinned hash/vector corpus is still used |
| Copy regular checkpoint bytes before writing (part of 315) | **Fixed**: `checkpoint.rs:1533`; regular bytes are borrowed, symlink encoding unchanged | 2,097,152 bytes copied for a 2 MiB regular blob | 0 bytes copied | Binary checkpoint restore matches all bytes; existing symlink, mode and durability tests remain green |

The durable read caches use Unix device/inode/size/mtime/**ctime**, including
nanoseconds, and check versions before and after a cache-seeding read. Resetting
mtime after rewriting a same-sized blob invalidates the cache. Missing blobs
still fail. Metadata is not inferred from cached entry ids. The cached regular
bytes are shared `Arc` buffers, not a second full copy. Cache ownership is
pruned with durable artifacts and cleared on namespace changes. Platforms
without the necessary change-time evidence use the original uncached path;
Windows runtime performance is not claimed.

## Completed-project checker validation

The initial delivery deferred checker coalescing to preserve intermediate-state
behavior. The operator subsequently approved a contract change for `apply_patch`
and `move_symbol`: with full validation enabled, run the checker **after every
write has completed**, not after each write. This section supersedes that deferral.

The write pipeline still formats and syntax-checks every write, preserving backup,
checkpoint, rollback and LSP notification ordering. Only the external type-checker
step is deferred. If any hunk fails (including a partially applied patch), or a
symbol move fails or rolls back, no checker runs. Single-file commands keep their
existing validation pipeline. The full-validation request-loop hook fires at the
final checker stage, after the complete operation's writes.

`format::validate_full_batch` deduplicates paths and groups by resolved checker,
arguments and configured project root. `tsc`/`tsgo` and `cargo check` are project
checks; Biome, Pyright and Ruff accept multiple file paths in one invocation.
`go vet` and Staticcheck accept named files only within one directory: same-directory
inputs are batched; cross-directory inputs **fall back to one invocation per file**
to avoid forming an invalid package or broadening scope. Project selection remains
the existing configured `project_root`; this change does not guess nested roots or
add a root cache. Detection happens after writes, so edited checker configuration
is visible. No subprocess result is reused between operations or project roots.

Parsed diagnostics stay on `metadata.files` for `apply_patch` and on the existing
per-file `results` for `move_symbol`. Clean files receive `validation_errors: []`;
skips receive the existing `validate_skipped_reason`. A nonzero exit with an error
in one touched file does not falsely mark the other touched files unchecked.
Move-hunk diagnostics attach to the final destination. `ValidationError.file`
retains the checker's reported path; the batch resolves it at the checker root
before attribution, so `one.ts` does not also match `nested/one.ts`. There is no
separate top-level list. Both commands emit one rendered `type check: …` summary; unchecked files are
explicitly counted with reasons. Checker errors do not undo successful writes.

The real TypeScript 5.9.3 integration fixture has three touched files and an export
whose consumer temporarily imports the old name. The old counting measurement was
replaced deliberately: its claim of three intermediate runs is now the opposite
of the approved contract, not an optimization-compatible invariant.

| Command / fixture | Before | After | Diagnostic evidence |
| --- | --- | --- | --- |
| Three-file `apply_patch`, rename an export and its consumer | 3 compiler runs | 1 compiler run | TS2305 exists only in the intermediate project; all final per-file arrays are empty |
| `move_symbol`, source + destination + consumer | 3 compiler runs | 1 compiler run | Not-yet-rewritten source import is never checked; completed project is clean |
| Either command, consumer assigns a returned number to `string` | Diagnostics discarded by handler | TS2322 on consumer entry only | Real compiler line 2 diagnostic plus `type check: 1 errors in 1 of 3 files (tsc)` |
| Partial patch / syntax-rolled-back symbol move | Could run during earlier writes | 0 compiler runs | Counter file absent; symbol move restores original source/destination bytes |

Three independent regression tests were observed red before implementation:
`multi_file_full_validation_runs_once_per_checker` (3 versus 1),
`multi_file_full_validation_reports_real_error_in_touched_file` (missing per-file
output), and `multi_file_full_validation_ignores_intermediate_only_errors`
(real TS2305). The latter inspects every captured compiler output, not merely the
rendered response, so dropping diagnostics cannot masquerade as final-state checking.
Separate mutation controls disable coalescing, discard parsed diagnostics, and
re-enable per-write checking; each has its own named red test. Unit fixtures also
exercise distinct tools, positional file batching, repeated paths, root isolation,
Go's fallback, exact root-relative diagnostic attribution, and honest skip reporting.
The exact-attribution guard was separately neutralized: only
`batch_validation_attributes_root_relative_diagnostics_exactly` failed; the other
63 format-library tests passed. All mutants were restored from the staged live
implementation, with a non-empty mutation diff and empty post-restore diff.

Run the real-compiler tests explicitly after `bun install`, under a throwaway
`HOME` and `XDG_CONFIG_HOME`/`XDG_DATA_HOME`/`XDG_CACHE_HOME`/`XDG_STATE_HOME`, with
the real `CARGO_HOME` and `RUSTUP_HOME` retained and `AFT_STORAGE_DIR` unset:

```
cargo test -p agent-file-tools --test integration -- multi_file_full_validation --ignored --nocapture
```

## Remaining claims checked in current source

The following residual counts are **source-operation counts**, not new runtime
counter measurements. They are unchanged by this delivery. These are deferred
findings, not claims that the entire slice is optimized or experimentally
closed. In particular, lower-priority ownership/locking and legacy SQL paths
still need their own fixtures and mutation-count fences.

| Audit finding | Current confirmation | Verdict / reason | Before → after work |
| --- | --- | --- | --- |
| Consumer walk/canonicalization/file reads (remaining 296) | `commands/move_symbol.rs:387,426,1052` | **Deferred**: named, namespace and wildcard re-export consumers must still be discovered, including aliases/symlinks. A source-basename filter could miss supported consumers. The implemented prefilter runs after reading bytes. | Walk/read each candidate file → unchanged; no runtime read-count claim |
| Hashline record/snapshot/store clones (remaining 301) | `hashline/scan/mod.rs:591-620`; `hashline/snapshot/mod.rs:576,656` | **Deferred**: owned public scan results, scanner publication and session snapshot history have distinct owners. Sharing requires a coordinated snapshot ownership refactor, not deleting a clone at one call site. | Record copies plus published/returned/stored snapshot copies → unchanged |
| LSP post-edit disk hash and stale-open checks (302) | `lsp/manager.rs:2021`, `lsp/document.rs:143-149` | **Deferred** under the manager fence. **Already fixed**: one shared `DiskSnapshot` per server fan-out rather than one disk read per server. Same-size/same-mtime drift checks still hash small documents for correctness. | One post-edit disk snapshot per path, conditional small-file hashing → unchanged |
| Whole-file TextDiff passes and no deadline (303) | `edit.rs:201,241,398` | **Deferred**: a deadline would change exact diff output. Sharing diff products between counts/rendering needs its own byte-identical formatting proof; the reflow diff also uses a different input pair. | Up to 3 distinct `TextDiff` constructions → unchanged |
| Snapshot metadata stat/write/fsync, sidecar and SQLite work (remaining 304) | `backup.rs:3966,3985,4028,4034,2455` | **Already fixed** for the audit's old flush picture: the durability rework commits content/meta renames with one directory flush and sidecar annotations are not durability commits. **Deferred**: no flush removed; retained-content stats, pretty metadata, sidecar annotation writes and two mirror phases remain. | Existing durability test: first Unix backup 6 syncs; steady backup 3 syncs → identical. Two annotation phases and mirror phases → unchanged |
| Regular backup bytes plus lossy String view (remaining 304) | `backup.rs:4594,4659,5202`; public `BackupEntry.content` | **Deferred**: replacing/removing the public String view changes the Rust API; restore must keep exact binary bytes. This change shares blob bytes but does not remove the required text view. | Bytes plus text view → unchanged |
| Sequential formatter per glob file and durable checkpoint + backup (305) | `commands/edit_match.rs:612,629,699`; `commands/lsp_rename.rs:326-362` | **Already fixed** for duplicate pre-edit *reads*: captures are shared by checkpoint and backup. **Deferred** for the two durable artifacts: checkpoint rollback and durable undo have different lifetimes; deduplicating their on-disk payloads needs a layout/lifetime change. Formatter batching also needs a formatter-specific argument/error contract. | One captured pre-edit read; one checkpoint blob and one undo blob per changed file; one formatter call per glob file → unchanged |
| Serial post-write LSP diagnostics in apply_patch (306) | `commands/apply_patch.rs:393`; no `multi_file_write_paths` assignment in the handler | **Deferred**: batching changes intermediate document synchronization and diagnostic freshness/order. Needs a multi-file LSP result contract and recent manager synchronization review. | One `lsp_post_write` call per written file → unchanged |
| Backup/binding mutex across hashline transaction (307) | `commands/hashline.rs:71,158-169` | **Deferred**: guards bind snapshot/register publication, undo and transaction rollback together. Shortening their lifetime requires a transaction/teardown concurrency proof; no locking change mixed into byte materialization. | One binding critical section and one backup critical section per transaction, including persistence → unchanged |
| Duplicate server resolution and per-file manager locking (308) | `commands/lsp_diagnostics.rs:689,707-711` | **Deferred**: server-list reuse is possible, but manager/root freshness and the cap envelope need a dedicated fixture before changing this separate diagnostics command. | 2 server-list resolutions per retained path; up to 200 paths → unchanged |
| Uncached LSP root resolution (309) | `lsp/roots.rs:91,161,220` | **Deferred**: workspace manifests/member globs can change between requests. A root cache needs manifest/config invalidation and should be coordinated with the manager rework. | Ancestor manifest reads/TOML parses and member matcher compilation per resolution → unchanged |
| Same-path hashline composition rebuild (310) | `hashline/apply/mod.rs:321,380` | **Deferred**: each operation remaps original coordinates and re-runs repair rules against the current baseline. Eliminating rescans needs an incremental baseline/repair equivalence proof, not treating composed operations as independent. | Baseline clones initially; one full baseline rebuild per composed operation → unchanged |
| LSP didOpen/didChange/didSave copies (311) | `lsp/client.rs:1990-2059`; `lsp/manager.rs:2059` | **Already fixed** for didChange/didSave parameter/JSON copies: borrowed serializers exist. **Deferred** for typed didOpen copy. IncludeText is negotiated server behavior, not a removable copy. Transport left unchanged. | Borrowed didChange/didSave; typed didOpen owned text; requested IncludeText still sent → unchanged |
| Patch owned lines / reverse splices (312) | `patch/apply.rs:152,170,232,258` | **Deferred**: the patch matcher has EOF, insertion and reflow semantics separate from hashline's materializer. Needs its own overlap/fuzzy-match counting and frozen output fixture; no speculative shared materializer applied. | One owned line buffer; reverse splice per hunk and line-reference rebuilds → unchanged |
| Ambiguity-path newline and context scans (313) | `commands/edit_match.rs:1234,1447` | **Deferred**, still reproducible from the source: per-occurrence prefix newline counting and full `lines()` collection remain. Requires an exact ambiguity-response/context fixture and counter, not covered by the success-path edit tests. | k prefix counts plus k full line-vector builds → unchanged O(k·n) |
| Write-ledger register allocation/mutex and never-pruned path entries (314 and audit 182) | `write_ledger.rs:259-286,396,480` | **Deferred**: removal must retain pending/cumulative attribution and live retained Counter handles; naive pruning changes census totals. Needs a retirement/folding protocol. | Global lookup and root-id allocation per credit; fold visits registered entries → unchanged |
| Per-section display baseline clones (remaining 315) | `commands/hashline.rs:130-136` | **Deferred**: display baselines are owned through transaction completion; changing them to shared/borrowed buffers needs transaction/render lifetime coverage. | One source display byte clone per section, plus destination clone when present → unchanged |
| Append reads/parses/diffs whole file (316) | `commands/edit_match.rs:242,313,319,357,381` | **Deferred**: undo, `no_op`, formatter-normalized diff and full-text LSP notifications require baseline/final content. The append path has its own error contract and does not use the shared write pipeline changed here. | Baseline/final whole-file reads, separate syntax read/parse and requested diffs → unchanged |
| Recursive delete per-entry persistence (audit 163) | `commands/delete_file.rs:461-495` | **Deferred**: entry snapshots retain independent durable commits, including directory/hard-link ordering. The audit's 4–5 sync estimate predates durability changes; batching commits would move required fsync points and is not permitted. | One snapshot and mutex acquisition per recorded entry; actual sync count depends on entry kind and first-use directories → unchanged |
| Backup SQL N+1 fallback queries (audit 266) | `backup.rs:2112,2209`; `db/backups.rs:97` | **Deferred**: both SQLite fallback loaders still query latest op, op rows and each affected stack. A batched SQL path needs ordered-row/partial-error/mirror concurrency tests, separate from authoritative disk hydration. | N+2 SELECTs for an N-path operation in each fallback helper → unchanged |
| Batch per-edit whole-string rebuild (audit summary 45) | `commands/batch.rs:122-125` | **Deferred**: still calls `replace_byte_range` per resolved edit. This legacy argument mode needs a counted frozen batch-output/overlap fixture before sharing the LSP builder. | E full-string constructions for E edits → unchanged O(E·n) |

## Safety and verification

All filesystem write helpers and fsync calls are unchanged. Existing durability
tests still check first-use parent publication order, steady sync counts, torn
sidecar handling, read-only restoration, missing/corrupt/future metadata,
checkpoint permissions, rollback, and retention. Compatibility tests for edit
materialization and durable history were not rewritten to accept opposite behavior;
the checker measurement's approved contract change is explained above.
Binary, Unicode, BOM, CRLF, EOF and malformed-range
compatibility controls accompany the counting tests.

Original delivery gates: Rust backup/checkpoint/edit/parser/hashline/move_symbol/rename lib
suites, the `aft` binary suite, integration move_symbol/rename/format suites, the
real-compiler measurement, and `cargo fmt --all -- --check`. The local Windows
cross-check was intentionally skipped as requested. Rust-analyzer inspection
was partial while its Cargo check was running; compiling lib, binary and
integration targets provided the authoritative local compiler check.

Checker follow-up gates: 64 format-library tests, 32 edit-library tests, four
real-TypeScript full-validation tests, 36 ordinary move-symbol integration tests,
15 patch integration tests, 35 format integration tests, 121 binary tests, and
`cargo fmt --all -- --check`. Cargo tests used throwaway HOME/XDG directories
without `AFT_STORAGE_DIR`, retaining the real Cargo/Rustup homes. Cargo was 1.99.0,
rustfmt 1.10.0-stable, Node 24.16.0 and TypeScript 5.9.3. The Windows cross-check
was skipped as requested. Scoped inspection was partial while rust-analyzer was
indexing; the test-target compilations provided authoritative type checking.

No ranking/routing fence files, configuration resolver, tool schemas, package
manifests or lockfiles changed. The prepared Bun install/build were not repeated
because no TypeScript package implementation changed. An initial combined
mutation run exhausted its 30-minute command cap while queued/recompiling; the
live files were restored from the staged index, and the remaining normalization
proof and compatibility controls were then run separately. No mutation was
committed.
