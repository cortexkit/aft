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

Pending implementation and native re-measurement.
