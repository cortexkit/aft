# Notes for the next release

- Daemon health: scheduler contention now preserves the last measured census and timestamp, marks it stale with an increasing age, and reports unmeasured startup metrics as null instead of empty/zero/available. The health sentinel treats stale or unknown census data as an instrument failure without losing watcher baselines or disabling live liveness checks.
- Access policy: heads can read their own workers' bash task records and status, including revived workers, without gaining task control; launch environments remain private. Sessions can grep and glob their own task I/O folders.
- Bash status: checking a session's own task no longer searches for worker records. When a primary agent checks a worker's task, AFT finds its owner in memory or the database instead of enumerating session folders. The added index keeps schema 18 and remains compatible with schema-18 database readers.
- Bash compression: command lists with top-level separators (`cargo test && cargo clippy`, `git add . ; git commit`) now use generic compression instead of the head command's specialized compressor. Previously the specialized compressor silently deleted every later command's output from the response on success — 26.65% of real bash traffic (7-day sample) was affected. Chained commands are now more verbose but complete.
- aft_outline / aft_zoom: signature lines no longer carry the redundant visibility/kind prefix (`E fn `) — the signature text itself is the information. A minimal `E` marker remains only where exported-ness is not visible in the signature (e.g. TypeScript export lists, Go uppercase exports). ~5.5% mean output reduction on outline, compounding on zoom menus.
- apply_patch: updates no longer eat one terminal newline from files ending with two or more.
- Windows LSP: percent-escaped file URIs (paths with spaces, non-ASCII) now decode correctly, so diagnostics/goto/rename work on those paths.
- edit: empty-string mode sentinels sent by some hosts no longer reject valid edits[] calls (issue #171).
- Workflow hints ship inside the host's system entry — fixes "System message must be at the beginning" on Qwen-family endpoints (issue #150).
- Checkpoints under restrict_to_project_root now snapshot a symlink itself rather than its target.
- Callgraph stores sweep orphaned build temporaries (age-based, both layouts).
- Worker agents that ask for a missing absolute path which exists in their own checkout now get "path not found: …; did you mean …?" instead of an access request to their head. A worker search of the filesystem root is refused with "use your checkout instead: <worktree>".
