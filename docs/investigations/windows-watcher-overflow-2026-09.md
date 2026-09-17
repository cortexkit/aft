# Windows watcher overflow during an Ally build (2026-09-17)

## Question and setup

This experiment measures whether one `cargo build -p agent-file-tools` in a watched checkout loses Windows filesystem notifications, whether AFT reports an error or rescan, and whether a tracked edit made during the build reaches the trigram index. Both measurements run natively on `asusallyko.local` while holding `C:\Users\ufuka\ally.lock`; cross-compilation is not used as runtime evidence.

The observer is a standalone AFT process configured with `search_index=true`, `semantic_search=false`, and `callgraph_store=false`. Its binary is copied out of Cargo's debug output before observation so the measured build can update `target/debug`. The measured command uses that already-populated target directory. Immediately before the command, the harness advances `crates/aft/src/lib.rs`'s mtime so the exact command performs a package rebuild. Two seconds after Cargo starts, the harness appends a unique marker to tracked `crates/aft/src/watcher_backend/mod.rs`. After Cargo exits it repeatedly requests `grep` for up to 30 seconds, then restores the file.

`Raw events` and `Kept after gitignore` are deltas of `status.watcher.raw_events_total` and `paths_after_gitignore_total`. Error visibility is the status degradation state plus the typed rescan counters.

## Before: notify 8.2 backend

Backend commit: `1ddc627dd766d92ba36f924406c64b46b4663f4c`.

| Backend | Build exit / duration | Raw events | Kept after gitignore | Notify error or typed rescan | Marker found after build | Trigram stale |
|---|---:|---:|---:|---|---|---|
| notify 8.2 `ReadDirectoryChangesW` | 0 / 77.469 s | 503 | 2 | none; all rescan counters 0 | yes, one match | no |

The watcher remained healthy (`degraded_reasons=[]`). It dispatched both kept paths and reported no overflow. This run therefore does not demonstrate a drop, but it does establish the baseline gap: notify's 16 KiB completion buffer produced no overflow signal for AFT to classify, and there is no recovery path if Windows does return `ERROR_NOTIFY_ENUM_DIR`.

A preceding attempt used a fresh Cargo target directory and is excluded from the comparison because Windows Application Control stopped Cargo with OS error 4551 (exit 101). Before that failed build exited it delivered 15,328 raw events, kept one path, reported no error or rescan, and indexed the marker. The successful table above uses the warmed, trusted target directory rather than treating the policy failure as watcher evidence.

## After: owned `ReadDirectoryChangesW` backend

Backend commit: `3197a989d`.

| Backend | Build exit / duration | Raw events | Kept after gitignore | Notify error or typed rescan | Marker found after build | Trigram stale |
|---|---:|---:|---:|---|---|---|
| owned `ReadDirectoryChangesW` + IOCP | 0 / 78.609 s | 470 | 2 | none; all rescan counters 0 | yes, one match | no |

The completion thread drained 467 root completions containing 472 translated events in this window. Cumulative opt-in samples reported **44 µs average** and **165 µs maximum** completion-to-channel drain latency. The timing includes copying the completed bytes, rearming the directory read, translating every entry, and sending the resulting events; it excludes downstream gitignore and index work. The read is rearmed before translation so that work cannot leave the recursive root handle without a pending 1 MiB kernel buffer.

The owned backend uses the larger reference allocation: parcel-watcher's 1 MiB starting buffer (`src/windows/WindowsBackend.cc:8-10,70-80` in the pinned reference snapshot). A network handle that rejects that allocation with `ERROR_INVALID_PARAMETER` retries at 64 KiB. `ERROR_NOTIFY_ENUM_DIR` and a successful zero-byte completion both emit a `Flag::Rescan` event tagged `rescan: buffer overflow`. The filter maps that tag to `BufferOverflow`, the existing drain coalesces it into a full-root rescan, and health exposes `rescans_buffer_overflow_total` alongside the three pre-existing reason totals. Raw, invalidating, kept, dispatched, and aggregate overflow accounting remains in the shared filter, so Windows has the same per-root telemetry path as FSEvents and inotify.

Because `ReadDirectoryChangesW` has no exclusion API, the root uses one recursive handle rather than per-directory handles. The completion buffer is copied and immediately rearmed before target-like paths reach the shared filter. External ignore files outside the root each use a non-recursive parent handle. Matcher generation is captured before backend startup and updated by the completion thread; the native race test pauses the thread before its first instruction and verifies that an intervening generation bump is not swallowed.

## Native Windows verification

All commands below ran on `asusallyko.local` while the mkdir lock and `HOLDER.txt` were held.

```text
cargo test -p agent-file-tools --lib watcher_backend::windows::tests:: -- --nocapture --test-threads=1
running 2 tests
windows_buffer_overflow_emits_exactly_one_typed_rescan ... ok
windows_generation_bump_before_backend_start_is_observed ... ok
test result: ok. 2 passed; 0 failed
```

The overflow test holds the completion consumer while 5,000 files are created with the test-only 256-byte buffer, then routes the native overflow through the production filter. It receives exactly one `RescanRequired(BufferOverflow)`. Replacing the buffer-overflow mapping with `Unknown` under the `NON-VACUITY BREAK` mutation made that exact test fail alone (`0 passed; 1 failed`); restoring the file returned the diff stat to empty.

```text
cargo nextest run -p agent-file-tools --test watcher_integration --no-fail-fast
Starting 23 tests across 1 binary
Summary [113.705s] 23 tests run: 23 passed (1 slow), 0 skipped
```

The required `scripts/ally-gate.sh 'test(watcher)'` invocation was also made under the lock. Its compile completed, but Windows Application Control blocked unrelated freshly emitted test-list binaries with OS error 4551 before nextest could apply the watcher filter. The package-scoped native tests above avoid that machine-policy enumeration failure while exercising the owned backend and the complete existing watcher integration binary.
