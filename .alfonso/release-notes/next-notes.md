# Notes for the next release

- Daemon health: scheduler contention now preserves the last measured census and timestamp, marks it stale with an increasing age, and reports unmeasured startup metrics as null instead of empty/zero/available. The health sentinel treats stale or unknown census data as an instrument failure without losing watcher baselines or disabling live liveness checks.
- Bash compression: command lists with top-level separators (`cargo test && cargo clippy`, `git add . ; git commit`) now use generic compression instead of the head command's specialized compressor. Previously the specialized compressor silently deleted every later command's output from the response on success — 26.65% of real bash traffic (7-day sample) was affected. Chained commands are now more verbose but complete.
- aft_outline / aft_zoom: signature lines no longer carry the redundant visibility/kind prefix (`E fn `) — the signature text itself is the information. A minimal `E` marker remains only where exported-ness is not visible in the signature (e.g. TypeScript export lists, Go uppercase exports). ~5.5% mean output reduction on outline, compounding on zoom menus.
- apply_patch: updates no longer eat one terminal newline from files ending with two or more.
- Windows LSP: percent-escaped file URIs (paths with spaces, non-ASCII) now decode correctly, so diagnostics/goto/rename work on those paths.
- edit: empty-string mode sentinels sent by some hosts no longer reject valid edits[] calls (issue #171).
- Workflow hints ship inside the host's system entry — fixes "System message must be at the beginning" on Qwen-family endpoints (issue #150).
- Checkpoints under restrict_to_project_root now snapshot a symlink itself rather than its target.
- Callgraph stores sweep orphaned build temporaries (age-based, both layouts).
- When the call-graph database is briefly busy, `aft_callgraph` says it is busy and to retry, and `aft_inspect` reports dead code as pending, instead of claiming no call graph exists. Background call-graph maintenance no longer gives up on a store it could not read.
