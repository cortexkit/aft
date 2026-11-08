# Bash, sandbox and compression performance slice

## Scope and method

Reviewed the audit's cross-cutting patterns and all 22 findings in its bash /
sandbox / compression section against base `14fb55e3b0e8e19cf7f58d50615ce9aefc1aa3a8`.
The verdicts below describe the current code, not just the audit's old line numbers.
Only the three High finding improvements described here are implemented. No
ranking-fenced files, configuration keys, tool arguments, list cuts, SQLite file
handles, or TypeScript packages changed.

Measurements count work rather than elapsed time. Test-only thread-local counters
observe the actual metadata read / JSON parse / open-attempt sites and the actual
sync and canonicalization calls. Each measurement resets after fixture setup.
Counters are absent in production builds.

## High findings

| Finding | Verdict | Measured work before → after | Pinned test |
| --- | --- | --- | --- |
| Artifact layout resolution and repeated metadata parsing | Confirmed; duplicate metadata read removed. Each access still resolves and validates the current layout rather than caching mutable metadata. | Per validated artifact open: **8 → 7 Unix open attempts**, **2 → 1 metadata reads**, **4 → 2 JSON parses**. The same reduction applies to a missing exit-marker poll. | `bash_background::persistence::tests::artifact_open_reads_metadata_once`; `bash_background::persistence::tests::missing_exit_marker_reads_metadata_once` |
| PTY status rereads the log and replays vt100 from zero | Confirmed; deferred by parent decision. | Unchanged: one full artifact read and one full vt100 replay per screen poll; not instrumented because no fix is proposed. | None; existing behavior retained. |
| About seven fsyncs per foreground task | Confirmed; remove four unnecessary payload syncs. Measured at the audit base: **7 → 3 fsyncs**. The per-store durability work landed first and already removed the three metadata syncs (task history is rebuildable), so on main this slice takes the lifecycle from **4 → 0**, and the module no longer has a flush helper at all. | Representative lifecycle with four payload files and starting/running/terminal metadata publications. Payload materialization alone: **4 → 0**. | `bash_background::persistence::tests::task_lifecycle_round_trips_payloads_and_metadata` (the sync counter was removed with the helper) |
| Native profile rebuilt per spawn | Confirmed; reduce duplicated work within each profile, not across spawns. Cross-spawn caching remains deferred. | Six-path missing credential floor, required project root and temp directory: **42 → 22 canonicalize calls per profile build**; **42 → 22** again during launcher revalidation. This is a profile-normalization measurement, not a claim about the entire spawn's syscalls or Git subprocesses. | `sandbox_profile::tests::native_credential_floor_normalizes_each_path_once_per_profile` |

### Artifact validation

The output fixture contains 130,000 bytes and metadata containing a roughly 5 KB
command. Open counts include the failed flat-layout probe. The existing resolver
already checks the schema and task identity through freshly pinned directories;
the subsequent second read did not validate the artifact itself. Removing that
second read leaves no-follow, link-count, duplicate-layout, layout identity and
artifact handle validation intact. The two retained JSON parses perform the
persisted-format gate and full task deserialization.

This is a reduction in work per artifact access, not elimination of all layout
opens during a two-stream render. Reusing directory/layout state across polls
would require proving that task replacement and metadata changes cannot escape
the existing validation contract.

### Payload visibility versus durability

The immutable command, wrapper, environment and manifest files are verified and
read through retained handles before the child is spawned. Writes already have
cross-process visibility without `sync_all`. Recovery does not re-execute a task
from these payload files. A power-loss guarantee for those files is unnecessary;
metadata still uses the synced atomic replacement path. The Unix lifecycle
fixture reads each returned payload handle to check byte-for-byte visibility,
and existing spawn, payload verification and replay tests exercise the consumers.

The lifecycle counter is scoped to these seven persistence sync calls. It does
not assert that a command causing unrelated database or user-file writes performs
only three syncs across the entire process.

### Native policy snapshot

The credential floor appears in both read and write deny lists. A per-build map
shares only successful deny-path normalization results, preserving sorted output
and error handling. Required grants still undergo their own existence/type checks.
The map is discarded at return; launcher revalidation uses a new map. The freshness
test `sandbox_profile::tests::deny_normalization_is_fresh_for_every_profile_and_launcher`
covers a missing deny path becoming a symlink, a new build resolving its new
target, launcher revalidation seeing that target, and a later build seeing the
path disappear again.

Git configuration/includes, symlinks, credentials, cache directories and Linux
read-grant enumeration can change between spawns. Their discovery remains fresh;
no mtime-only or global profile cache has been introduced. The broader cost of
per-spawn Git queries and directory enumeration is not claimed fixed.

### Deferred PTY design and decision

The parent explicitly chose to defer this High finding. An exact-prefix replay
cache would still read and compare the whole log on every poll and keep another
potentially large copy, trading vt100 CPU for memory without removing disk work.
A size/mtime shortcut cannot reliably detect prefix edits to the child-writable
log.

The design that actually removes the work is for the PTY runtime that writes the
log to own a live vt100 screen. Status reads that screen; replay from disk is only
the fallback after a module restart. This changes semantics: after the disk cap
trims a log, or a child edits its own log, status would show true terminal state
instead of replaying the trimmed/edited file. That behavioral decision belongs
in a separate change. Neither approach is implemented here.

## Optional findings: confirmed and skipped

These sites were read against the current code. No improvement or measured
before/after claim is made for skipped items. References name the live symbols so
the findings remain locatable when line numbers move.

| Finding | Verdict and current site | Work before → after | Reason skipped |
| --- | --- | --- | --- |
| Full `/proc` scan at pipes completion (MH) | Confirmed: `process::linux_process_group_members`, used by process-group descendant inspection. | Unchanged; not measured. | Removing enumeration needs a Linux process-tracking design that preserves descendant reporting and kill safety; Linux runtime is unavailable on this macOS worker. |
| Pipes-mode completion polling (MH) | Confirmed: `watchdog::WATCHDOG_INTERVAL` is 500 ms; `registry::poll_task` checks the exit marker and `reap_child` uses `try_wait`. PTY already has reader/waiter wakes, but pipes do not have that coordinator. | Unchanged; not measured. | A waiter redesign must preserve descendant-held pipes, restored tasks and kill/reap ownership. |
| Task/global lock scope (M) | Confirmed: metadata and artifact work remains in task-state transactions; `registry::running_count` calls `task.is_running()` while holding the task map lock. | Unchanged; not measured. | Moving persistence requires maintaining transition ordering; fixing admission/health lock scope is a separate concurrency change. Existing DB writes already have a sequencing fence and must not be reordered casually. |
| Compressor mutex held through compression (M) | Confirmed: `registry::compress_output` invokes the callback through the locked slot. | Unchanged; not measured. | Optional concurrency improvement left outside this High-first implementation; replacement and poisoned-lock behavior need their own lock-scope tests. |
| Whole-file head/tail rewrite reads (M) | Confirmed: `rules::head_tail_shape_is_faithful` reads the file and collects its lines; tail's `text_file_line_count` adds another whole-file read. | Unchanged; not measured. | Streaming faithfulness must preserve UTF-8, line-count and native fallback behavior rather than adding an output-changing cap. |
| Find rewrite prewalk and canonicalization (M) | Confirmed: `rules::find_shape_is_faithful` canonicalizes, directly walks candidates, and invokes glob to compare candidate sets before dispatch. | Unchanged; not measured. | Removing the preflight can change native/rewrite routing and requires separate differential coverage. |
| Artifact range reads/base64 to EOF (M) | Confirmed: `registry::read_artifact_range` reads `len - offset`; `commands::bash_status::handle` encodes it. | Unchanged; not measured. | A cap changes the public chunk/next-offset contract, contrary to byte-identical output in this slice. |
| Watch UTF-8 revalidation per match (M) | Confirmed: `watches::find_match` calls `source_offset_for_lossy_utf8` for match boundaries, which runs `from_utf8` again. | Unchanged; not measured. | Lossy-UTF-8 offset mapping needs a shared offset representation and invalid-byte regression tests. |
| Governance hook revalidation per spawn (M) | Confirmed: `agent_child_env::ensure_managed_git_hooks` quarantines entries and checks/refreshes hook contents each use. | Unchanged; not measured. | These checks protect mutable executable hooks; caching requires an exact integrity/invalidation contract. |
| `ROUTE_RECORDS` retention (M) | Confirmed: `dispatch::store_record` inserts per request; `take_route_record` exists but production consumers do not drain it. | Unchanged; not measured. | Retention policy and diagnostics consumers require a separate lifecycle change; no arbitrary eviction added. |
| systemd launcher probe per opt-in spawn (M) | Confirmed: `process::select_systemd_scope_launcher` probes `systemd-run --user --scope ... /bin/true` when `linux_scope` is requested. | Unchanged; not measured. | Caching user-manager availability can go stale; Linux runtime cannot be verified here. |
| Completion-path BPE statistics (LM) | Confirmed: `registry::completion_token_counts` synchronously counts raw and compressed output, with a 128 KiB per-stream input budget. | Unchanged; not measured. | Offloading affects completion/statistics ordering and needs a bounded job ownership design. |
| Watch-target DB checks and cursor dedupe order (LM) | Confirmed: `evaluate_erased_watch_targets` queries candidates on watchdog passes; `persist_task_watch_cursors` acquires the DB lock before checking unchanged offsets. | Unchanged; not measured. | Watch-row erasure and durable cursor semantics need focused concurrency/DB tests; no polling cadence change. |
| Pending-watch row filtering (L) | Confirmed: `stuck_pending_watches_for_session` loads session watch rows and filters pending/age in Rust. | Unchanged; not measured. | Optional SQL pushdown not needed for the High fixes; preserve task-presence and age semantics in a separate change. |
| Artifact ownership scans retained tasks (L) | Confirmed: `is_session_owned_artifact_path` and `read_artifact_path` inspect task-map values. | Unchanged; not measured. | An inverse index needs exact registration/removal/replay maintenance to preserve path restrictions. |
| Status miss causes replay (L) | Confirmed: `status_with_kill_wait` replays the session on an in-memory miss. | Unchanged; not measured. | A negative cache can hide newly created/adopted tasks unless every relevant transition invalidates it. |
| End-only disk-cap rewrite (L) | Confirmed: `persistence::replace_artifact_with_tail` reads the retained tail and atomically republishes it during terminal output capping. | Unchanged; not measured. | Incremental rotation changes artifact offsets and PTY replay semantics; defer with the PTY design. |
| Regex/DSR/permission scan setup (L) | Confirmed: `bash_regex_match::regex_match` compiles each pattern; `pty_process::DsrScanner::scan` copies carry plus chunk; `bash_permissions::scan` creates a parser and `mod::temp_roots` canonicalizes roots. | Unchanged; not measured. | Optional micro-optimizations need their own counters; permission-root reuse also requires exact filesystem freshness. |

## Non-vacuity evidence

Each counter test was shown failing on the original implementation, then passing
with the fix. For isolated mutation proofs, the live files were staged first,
`git diff --stat` was empty, the mutation was marked `NON-VACUITY BREAK`, the
non-empty diff was captured, and each named test ran independently. Files were
restored with `git checkout -- <path> && touch <path>` and the unstaged diff was
empty again. No mutation was committed.

| Control | Exact red test | Red count / result | Mutated diff → restored diff |
| --- | --- | --- | --- |
| Reintroduce the duplicate metadata read | `bash_background::persistence::tests::artifact_open_reads_metadata_once` | Reads 2 rather than 1; opens 8/parses 4; 0 passed, 1 failed. | `persistence.rs`: part of 2 insertions in 1 file → empty. |
| Same duplicate-read control, missing-marker path | `bash_background::persistence::tests::missing_exit_marker_reads_metadata_once` | Reads 2 rather than 1; 0 passed, 1 failed. | `persistence.rs`: part of 2 insertions in 1 file → empty. |
| Restore control-payload `sync_all` | the lifecycle sync counter (removed on main with the flush helper) | At the audit base: syncs 7 rather than 3; 0 passed, 1 failed. | `persistence.rs`: part of 2 insertions in 1 file → empty. |
| Clear deny-path reuse before every lookup | `sandbox_profile::tests::native_credential_floor_normalizes_each_path_once_per_profile` | Canonicalizes 42 rather than 22; 0 passed, 1 failed. | `sandbox_profile.rs`: 1 insertion in 1 file → empty. |

All mutation runs had exactly the named test fail and no other failures. Final
restored module and integration runs were green.

## Gates

Tool versions: `cargo 1.99.0 (5f94df478 2026-08-27)`,
`rustc 1.99.0 (b940084d7 2026-09-28)`,
`rustfmt 1.10.0-stable (b940084d7e 2026-09-28)`.

| Exact command | Result |
| --- | --- |
| `cargo test -p agent-file-tools --lib -- bash_background::` | 140 passed, 0 failed. |
| `cargo test -p agent-file-tools --lib -- sandbox_` | 40 passed, 0 failed (includes 5 profile tests). |
| `cargo test --features test-hooks -p agent-file-tools --test integration -- bash` | 326 passed, 0 failed, 1 ignored. |
| `cargo test --features test-hooks -p agent-file-tools --test integration -- sandbox` | 6 passed, 0 failed. |
| `cargo test -p agent-file-tools --bin aft -- sandbox` | 8 passed, 0 failed. |
| `cargo fmt --all -- --check` | Exit 0; silent-success gate. |
| `RUSTFLAGS="-D warnings -A deprecated" cargo check -p agent-file-tools --tests --target x86_64-pc-windows-gnu` | `Finished` successfully; compile-only, not Windows runtime evidence. |

The initial Windows check caught the Unix-only wrapper constant in the new
lifecycle fixture. The fixture was gated with `cfg(unix)`, Windows compilation
passed, and its Unix test was rerun: 1 passed, 0 failed. This did not change
production code. macOS linking reported an existing oversized `__eh_frame`
warning; tests passed. `aft_inspect` could not provide authoritative Rust
diagnostics within its budget; the compiler/test gates above provide verification.
List-envelope was not run because no list-cutting site changed. Linux native
runtime and Windows runtime execution were unavailable on this macOS worker.
