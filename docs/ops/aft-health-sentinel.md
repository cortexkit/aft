# AFT health sentinel

`bun scripts/sentinel/aft-health-sentinel.ts --once` is a short-lived external observer for the AFT daemon. Launchd runs it every 120 seconds; it does not keep a probe process or polling loop alive. `--once --dry-run` prints the complete sample and every detector finding without changing state or delivering alerts.

State is stored in `$AFT_SENTINEL_STATE_DIR/state.json`; launchd sets this to `~/.local/state/cortexkit/aft/sentinel`. Manual runs default to the disjoint `sentinel-dev` directory, and tests create a temporary directory, so development cannot consume or reset the live cooldown ledger. Raised and cleared transitions are appended to `findings.jsonl`; this file's mtime is the integration arm used by ALF. A fingerprint identifies the subject, not its current age or count. A still-present CRITICAL finding re-alerts after 30 minutes and a WARNING after two hours. Missing fingerprints are removed from state, logged as cleared, and alert immediately if they recur.

The collector reads `ck module status aft --json`, which exposes the same ManagementSurface `health.check` report as `subc-probe --health-probe aft`. It also reads only new bytes from the current `aft-<pid>.log` and OpenCode plugin log, accounting for rotation; process and filesystem counters; storage sizes; and the running image's LC_UUID and dSYM. Every unreadable input raises `instrument:<name>` rather than silently disabling a detector. A serving daemon with zero new pid-log lines raises `instrument:log-silent`.

## Rules

| Rule | Severity and trigger | Clears when |
|---|---|---|
| `daemon.down` | CRITICAL when the supervisor reports AFT not running or cannot be read; a failed health probe with a live supervisor behind it is an `instrument:health-check` warning (3 consecutive ticks), never this page | health is reachable and the supervisor reports AFT running |
| `daemon.restarted` | WARNING when the pid changes without a placement record; the text includes old/new pid and exit code | the next run sees the same pid |
| `daemon.panic` | CRITICAL on a new `panicked at`, `actor_fatal`, or `fatal executor` line | the next log window has no panic signature |
| `limiter.saturated` | CRITICAL with at least five cold-build deferrals and no slot acquisition in 15 minutes | a slot turns over or the window has fewer than five deferrals |
| `index.stuck` | CRITICAL per root and plane when `search_index`, `semantic_index`, or `tier2` remains `building` over 10 minutes and its progress timestamp is also older than 10 minutes | status becomes ready or progress advances |
| `tier2.pass_overlong` | WARNING when an inspect Tier-2 slot has no matching `perf tier2 phases` completion after 10 minutes | the completion is logged or the pass is cancelled |
| `bind.stall` | CRITICAL for a new `did not answer route.bind within` line, fingerprinted by root | the next window has no timeout for that root |
| `executor.phantom` | CRITICAL when maintenance remains in flight with every worker idle across two samples, or any zombie reader is reported | in-flight work agrees with active workers and zombie readers are zero |
| `wakes.backlog` | CRITICAL when the oldest unacked completion exceeds five minutes, or the plugin log contains `promptAsync`/`this._client` delivery failures | age is at most five minutes and no delivery failure occurs |
| `sandbox.refusal` | WARNING for a new `sandbox setup for … failed` line; the cause is retained verbatim | the next window has no refusal for that root |
| `watcher.overflow` | WARNING when kernel- or user-dropped rescan counters increase; root and overflow prefixes are included | neither counter increases in the next interval |
| `disk.low` | CRITICAL below 40 GiB free, WARNING below 80 GiB | free space reaches the corresponding threshold |
| `storage.growth` | WARNING when `aft.db`, `inspect`, `callgraph`, `blobs`, `views`, or `logs` grows over 5 GiB in one interval | the next interval grows by at most 5 GiB |
| `process.footprint` | WARNING above 6 GiB physical footprint | footprint is at most 6 GiB |
| `process.cpu` | WARNING above 150% CPU averaged over the interval | interval CPU is at most 150% |
| `process.writes` | WARNING above 1 GiB/hour writes; includes the three largest growing cache-key artifacts, their mapped roots and share of the write delta, and calls out likely in-place/WAL churn when growth explains under half | write rate is at most 1 GiB/hour |
| `search.degraded` | WARNING per root when more than 20% of at least five search calls disclose `fully_degraded` or `index: building` | degraded share is at most 20% |
| `tool.slow` | WARNING per root at the onset of a slow episode (more than 10 completed calls over 10 seconds in one tick); quiet while the episode continues, since every counted call has already finished | a tick with at most 10 slow calls ends the episode |
| `routes.dead_sessions` | WARNING when memory census reports bound routes on a root idle beyond its configured root TTL | routes close or root activity is newer than the TTL |
| `dsym.missing` | WARNING when the running image's LC_UUID has no artifact under `aft/dsym/<UUID>/` | an artifact is stored at that UUID key |
| `dsym.stale` | WARNING when an artifact exists at the running UUID key but its own LC_UUID differs; the running and found UUIDs are named | the artifact at the key has the running image's LC_UUID |

## Delivery

Every raised and cleared transition is written as `{ts, rule, state, fingerprint, text, severity}` JSONL. Raised actionable findings are sent through the registry sentinel's `peer.enqueue_message` wire shape with an `AFT` prefix. Daemon-down and unreachable-health findings also use a macOS notification because the peer path may be unavailable. A peer delivery failure is itself appended as `instrument:peer-delivery`.

## Install and inspect

The checked-in launchd definition is machine-specific because it must execute the primary checkout, not a disposable worktree:

```sh
mkdir -p ~/.local/state/cortexkit/aft/sentinel ~/Library/LaunchAgents
cp scripts/sentinel/com.cortexkit.aft.health-sentinel.plist \
  ~/Library/LaunchAgents/com.cortexkit.aft.health-sentinel.plist
launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/com.cortexkit.aft.health-sentinel.plist 2>/dev/null || true
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.cortexkit.aft.health-sentinel.plist
launchctl print gui/$(id -u)/com.cortexkit.aft.health-sentinel
```

For a manual sample:

```sh
bun scripts/sentinel/aft-health-sentinel.ts --once --dry-run
bun scripts/sentinel/aft-health-sentinel.ts --once
tail -n 20 ~/.local/state/cortexkit/aft/sentinel/findings.jsonl
```

The fixture test replays `~/.local/share/cortexkit/aft/wedge-specimens/2026-09-17-tier2-spin/` and requires `limiter.saturated`, `index.stuck` for magic-context, and `tier2.pass_overlong` for the affected worktree. `scripts/sentinel/fixtures/healthy-limiter.log` preserves the post-restart baseline of 37 acquisitions and one deferral in two minutes; it must not trip saturation.
