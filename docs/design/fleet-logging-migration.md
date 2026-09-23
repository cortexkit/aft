# AFT on the fleet logging convention: migration design

Status: design only. No product code changes here.
Rule: SUBC `#fleet-notices` #609, which makes the r2 spec mandatory
(`subconscious/docs/specs/fleet-logging.md`). Each module writes
`<data dir>/logs/<module>.<YYYY-MM-DD>.log` through `cortexkit-log`.

Sources read for this note:

- `cortexkit-log` 0.3.1: commons commit `b13e7fc`, files
  `crates/cortexkit-log/src/{lib,segment,format,redaction,sink}.rs`. The
  current tip is 0.3.2 (`4bfcb49`). It adds `Handle::emit_at` and
  `installed()` and nothing else.
- The TS twin `@cortexkit/log` 0.2.0: `subconscious/clients/log/src/index.ts`.
  Version 0.2.0 is on npm.
- AFT: `crates/aft/src/logging.rs`, `log_ctx.rs`, `main.rs`,
  `packages/aft-bridge/src/{durable-log,bridge}.ts`,
  `packages/opencode-plugin/src/logger.ts`, `packages/aft-cli/src/**`,
  `scripts/sentinel/`, `scripts/telemetry/`.

Tags used below: **[src]** means verified from source. **[obs]** means
observed on the operator's machine on 2026-09-23. **[unverified]** means an
inference I did not check.

---

## 1. What cortexkit-log 0.3.1 provides

| Property | Behaviour | |
|---|---|---|
| File name | `<logs_dir>/<module_id>.<YYYY-MM-DD>.log`. The date is the **UTC** day of each write. The file is never renamed. `segment_name`, `segment_day` (segment.rs). | [src] |
| Rotation | None by size. The writer compares the day on every write and reopens when the day rolls (`SegmentDestination::write`). When a segment passes `alarm_segment_mb` (default 256 MiB) the crate writes one stderr line per process, `segment oversized, NOT truncated`. It never truncates. | [src] |
| Retention | Age only. `max_age_days` defaults to 14. Every writer prunes when it opens a segment and at each day roll. The prune compares file names only (`prune_candidates`). A name that is not exactly `<module>.<date>.log` is never touched, so the old `aft-<pid>.log` files survive the prune. A lost unlink race (`ENOENT`) is ignored. There is no directory byte budget. | [src] |
| Concurrent writers | No lock and no coordinator. The file is opened `create+append` (`O_APPEND`). Each line is one `write_all` of `line + "\n"` under a **per-process** `Mutex`. The comment at the site: "the mutex keeps this process's lines whole against its own threads; O_APPEND keeps them whole against other processes." There is no `flock`. Two limits follow. (a) `write_all` loops on a short write, so a partial write (disk full, signal) can tear a line. (b) POSIX makes the seek-to-end and the write atomic, but does not formally promise that concurrent writes to a regular file never interleave. Local APFS and ext4 serialize them in practice; NFS does not. The Windows append behaviour is not checked. | [src]; the interleaving limits are [unverified] |
| Write path | Synchronous, on the thread that logs: "No queue, no background flusher". There is no channel and no dropping. When a write fails, the crate bumps `swallowed_writes` and reports the first failure per process to stderr. | [src] |
| Permissions | The file is created `0600`, then `chmod 0600` after every open, so an existing `0644` file gets fixed. The logs directory is `chmod 0700` on every open (`prepare_dir(.., enforce_directory_mode=true)`). Windows gets no mode handling. | [src] |
| Line format | `<ts> <LEVEL padded to 5> <logger>: [<bound k=v …>] <message> <k=v fields>`. Timestamp: RFC 3339 UTC with millisecond precision, e.g. `2026-09-19T07:59:01.882Z`. The bracket is left out when nothing is bound. `\n`/`\r` in the message are escaped, and values containing a space, `"`, `]` or a newline are quoted. ANSI and C1 sequences are stripped. Exactly one line per record. | [src] |
| Logger names | `<module>` or `<module>.<component>`. A component must match `[a-z][a-z0-9-]*`. A `tracing` target that contains `::` (any Rust module path) **maps to the bare module id**. | [src] |
| Filtering | `CK_LOG` uses the RUST_LOG grammar over logger names and defaults to `info`. An invalid spec falls back to `info` with one stderr note. `RUST_LOG` is not read. | [src] |
| Redaction | Built in, applied to the whole line: `Authorization:` values, `Bearer …`, JWT shapes, `ckh_…`, `sk-…`, `ghp_`/`gho_`. An optional module `Redactor` runs after it. **Not covered:** `github_pat_…`, `ghs_`/`ghu_`/`ghr_`, and userinfo in URLs (`https://user:pass@host`). | [src] |
| Panics | The crate installs a panic hook. It writes the panic text and a **forced backtrace**, one line per source line, at `ERROR` on logger `<module>.panic`, then calls the previous hook (the default one prints to stderr). | [src] |
| Fallback | If the directory cannot be opened, every line goes to stderr. The first line announces the fallback. | [src] |
| Self-reports | Retention (`<module>.retention pruned=N kept=M`), oversize and write-failure notices go to **stderr only**, not into the segment. | [src] |
| Init | `init(Config) -> Result<Handle, InitError>`, once per process. It installs `Registry::default().with(layer)` through `tracing::subscriber::set_global_default`. The layer is **not** public, so a module cannot add a second layer (for example a stderr copy) under 0.3.1. Constructors: `Config::in_dir(module_id, logs_dir)`. With the default `store-paths` feature there are also `for_module(id)` (which uses `module_data_dir(id)/logs`), `for_plugin(id, harness)` and `from_env()`. `from_env()` needs `SUBC_MODULE_ID` and reads `CK_LOG_MAX_AGE_DAYS` and `CK_LOG_ALARM_SEGMENT_MB`. `Config.bound` holds process-level `k=v` pairs rendered on every line. `session_span(issuer, id)` binds `session=<issuer>:<id>` to every event inside the span. | [src] |
| `log` crate | **Not bridged.** The crate installs no `tracing_log::LogTracer`, so `log::info!` records never reach the layer. Bridging them with `LogTracer` would also add `log.target=`, `log.module_path=`, `log.file=` and `log.line=` as fields on every line: tracing-log 0.2.0 (`lib.rs:254-257`) declares those fields, and the crate's `EventVisitor` renders every non-`message` field. | [src] for both halves; the combined effect is [unverified] until a test covers it |
| Distribution | **Not on crates.io.** `cargo info cortexkit-log@0.3.1` fails against the crates.io index. The crate exists only in the commons git repo. | [src]/[obs] |

## 2. AFT processes and which of them write the daily file

Today `logging::init()` runs in `main.rs:152` for every entry point that
reaches it. Each such process creates `aft-<pid>.log`, runs a directory sweep,
and copies every line to stderr (`TeeWriter`). On the operator's machine
(2026-09-23), 738 of the 753 files in `logs/` are under 4 KB. Nearly all of
them come from `aft profile --writes`, which the health sentinel runs every
2 minutes (`aft-health-sentinel.ts:1131`) [obs].

| Process | Logger today | Proposal |
|---|---|---|
| Daemon, `aft --subc <conn>` | init + tee to stderr. Under the daemon, stderr lands in `run/logs/aft.stderr.log`. | **Durable.** Bound `pid=<n> role=daemon`. No stderr copy: the spec treats an r2-shaped line in the capture file as a defect. |
| Standalone bridge, `aft` over stdin NDJSON (one per harness bridge/project) | init + tee. The TS bridge **also relays** every stderr line into `aft-plugin.log` (`bridge.ts:1475-1485`, `logVia`), so each line is stored twice. | **Durable.** Bound `pid role=bridge harness=<h>`, with the harness passed by the bridge in an env var at spawn. The bridge keeps its in-memory stderr tail for crash messages but **stops persisting** relayed lines, because they would land twice in the same segment. |
| `aft warmup` | init (after `main.rs:152`) | **Durable**, `role=warmup`: it builds indexes in-process, and a failure there is worth keeping. |
| `aft profile` (memory/CPU/`--writes` census) | init | **stderr only**, never a file. It is a read-only probe that runs every 2 minutes. |
| `aft index`, `migrate-storage`, `sandbox-launch`, `--probe-login-shell-path`, gh shim (`gh …` via argv0/env) | Exit **before** `logging::init()` (`main.rs:62-139`), so `log` macros are no-ops. | Unchanged: no logger. The gh shim in particular must stay logger-free, because it runs in agent shells with `gh` arguments and tokens. |
| `aft doctor` | This is the **TS** CLI (`packages/aft-cli`). There is no Rust `doctor` subcommand (`crates/aft/src/cli/` has index/profile/warmup/probe/sandbox_launch). | Reader only (§6). |
| Test binaries | Isolated by `AFT_STORAGE_DIR`. | Unchanged. |

**Telling processes apart.** A shared file loses the pid that the old file
name carried. Proposal: bind `pid=<n>` and `role=<daemon|bridge|warmup>` as
process-level `Config.bound` pairs, then `harness=` for bridges. The session
goes in a span via `session_span` (§3, "session tag"). The spec does not define
`pid` or `role`. They are AFT-local bound keys and should be named in AFT's
fleet census row.

## 3. AFT's own logging guarantees and where each one goes

| logging.rs guarantee | Where it goes |
|---|---|
| 32 MB size rotation with 1 backup (`LOG_FILE_BYTES`, `LOG_GENERATIONS`) | **Replaced** by the daily segment plus the 256 MiB alarm. Losing the size cap is a trade the fleet accepted on purpose. Largest AFT pid log today: 6.8 MB [obs], well under the alarm. |
| Dead-PID reap after 24 h (`DEAD_PROCESS_LOG_MAX_AGE`) | **Unnecessary** for new files, which carry no pid. **Kept temporarily** as a one-shot legacy sweep of `aft-<pid>.log[.1]`, because the crate's prune ignores those names [src]. Delete the sweep one release after the cut plus 14 days. |
| 200 MB directory budget (`LOG_DIRECTORY_BUDGET_BYTES`) | **No equivalent.** The crate prunes by age only, and deleting a live shared segment would break other writers. Decision needed: accept age-only retention (the fleet default is 14 days; files modified in the last 24 h total 28 MiB [obs], which puts a 14-day window near 400 MB) or set `max_age_days` lower in `aft.jsonc log.*` / `CK_LOG_MAX_AGE_DAYS`. The budget stays in force for legacy files only until they are gone. |
| Recycled-PID detection (`pid_started_after_last_write`) | **Unnecessary**: nothing is keyed on pid liveness any more. |
| Keep the newest dead log for forensics | **Subsumed.** A dying process's last lines are in today's segment, and the segment stays for `max_age_days`. Nothing deletes a segment inside the window. |
| Relic `aft-plugin.log.N` (N ≥ 2) reap | Legacy sweep only, as for pid files. |
| Hourly sweep from maintenance ticks (`maybe_sweep_logs`) | **Replaced** by the writer's prune at open and at each day roll. |
| **File I/O never on request, watcher, executor or transport threads** (bounded channel, dedicated `aft-log-writer` thread, drop when full, `PERF.file_lines_dropped`) | **Not provided.** The crate writes synchronously under a mutex on the calling thread. Either **keep locally** or accept the change after measuring. See S3, where it is a gate. Under the 250–385 load average seen on 2026-09-22 (sentinel comment), a blocking `write(2)` on the executor thread is a real risk [unverified in size]. With 0.3.1, keeping it off-thread means AFT's queue thread emits the `tracing` event. That loses the caller's timestamp and span (0.3.2's `Handle::emit_at` keeps the timestamp but binds no span fields). A clean fix needs a crate API. |
| `file_lines_dropped` in `perf tick` | Map it to `Handle::swallowed_writes()` if writes stay synchronous. The meaning changes from "queue full" to "write failed". |
| Write-ledger credit `Domain::Logs` (`RotatingFile::write_batch`, which feeds `aft profile --writes`) | **Not provided.** Keep it locally: count the rendered bytes in AFT's `log`→`tracing` bridge, or in the module `Redactor`, which sees every complete line. Otherwise log bytes silently disappear from the writes census. |
| Moving the storage root at configure time (`sync_storage_root`, `LogMessage::Reconfigure`) | **Not provided**: `logs_dir` is fixed at `init`. Resolve the root before `init`. The bridge already honours `AFT_STORAGE_DIR` and can pass the configured root in the env at spawn. The spec says the same thing: "one resolves it and the other is told". Use `Config::in_dir(storage_dir()/logs)`, **not** `for_module("aft")`: AFT's `storage_dir()` honours `AFT_STORAGE_DIR`/`AFT_CACHE_DIR` and differs on Windows, and the `store-paths` feature would pull in `cortexkit-store-types` for nothing. |
| stderr tee | Dropped in subc mode (the spec requires it). Standalone: see the bridge row in §2. 0.3.1 cannot add a stderr layer, so any stderr copy must come from AFT's own `log::Log` bridge. |
| `[aft]` / `[aft-lsp]` prefix by target | Becomes the logger names `aft` and `aft.lsp` (plus later `aft.index`, `aft.perf` and so on). This needs **fixed** targets: a Rust module-path target collapses to `aft` [src]. |
| Session tag `[ses_xxx]` in the message (`log_ctx::session_prefix`, including the "last session on this thread" fallback) | Becomes `session=<issuer>:<id>` in the bracket through `session_span`. The issuer (`opencode`/`pi`/…) is not known inside `log_ctx` today and has to come from configure/bind. The last-session fallback attributes a line to a session it may not belong to. Under a match-key field that is worse than today; drop it or bind it under a different key. |
| Seconds-precision timestamp without a level column | Becomes millisecond RFC 3339 **with** a level column. This breaks two scripts (§6). |
| `RUST_LOG` filtering (env_logger, default `info`) | Becomes `CK_LOG`. Keep a documented compatibility read of `RUST_LOG` for one release. Old specs such as `aft::lsp=debug` will not match logger names. |

**Recommended Rust shape (S3).** Keep AFT's existing `log` call sites and
`slog_*` macros. Install an AFT-owned `log::Log` implementation that:
maps each record to a fixed logger name; enters `session_span` from `log_ctx`;
emits `tracing::event!` at the matching level (no `LogTracer`, so no `log.*`
fields); credits the write ledger; and, for standalone only, optionally copies
WARN and above to stderr. This leaves every existing call site untouched.

## 4. Content review

Method: grep-based, over `crates/aft/src` log macros and the TS plugin
loggers. The review looked for bash command text, tool arguments or request
bodies, file contents, tokens, and URLs with credentials. It is **not an
exhaustive audit of every multi-line macro site** [unverified beyond the list].
Today's files are `0644` inside a `0755` directory [obs], so moving to
`0600`/`0700` is an improvement in itself. The risk that remains is
*retention*: 14 days of one shared file instead of per-pid files reaped after
24 hours.

| Site | Level | Content | Proposal |
|---|---|---|---|
| `main.rs:469` `slog_error!("parse error: {} — input: {}", e, trimmed)` | ERROR (on by default) | The **full raw NDJSON request line** on a malformed standalone request. It can contain bash `command` text, `write`/`edit` file contents, and search queries. | **Must change before adoption.** Log the error, the byte length and a hash; drop `input`. |
| `url_fetch.rs:312` URL rewrite `{original} -> {rewritten}` | debug | Full URLs, which may carry query tokens or userinfo. | Keep at debug. Add URL userinfo and `token=`/`access_token=` query redaction to AFT's module `Redactor`. |
| `runtime_drain.rs` ~3300–3329 `[aft-lsp] notification/request … {params}` | debug | Full LSP server params. Diagnostics can quote source; `workspace/applyEdit` carries file text. | Keep at debug. Truncate params to N bytes. |
| `sandbox_spawn.rs:1152-1155` sandbox setup failed `{cause}` + `session=` | warn | Error text and paths. | Acceptable (paths are fine). |
| `format.rs:1592` `format: {path} ({cmd})` | info | Formatter name, not user shell text. | Acceptable. |
| `subc/mod.rs:2676` authenticated via `{connection_file}` | info | A path. The token lives in the file and is not logged. | Acceptable. |
| bash (`bash_background/`, `commands/bash*.rs`), gh shim | — | No info-or-higher site found that logs command text. The gh shim runs before logger init. | None. Pin with a test that a bash call's command text does not reach the segment at the default level. |
| LSP failure reasons that embed a server stderr tail (`lsp/manager.rs:2191`, `:2927`) | — | Third-party server stderr, bounded by `STDERR_REASON_BYTES`. Whether this reason is logged, and at what level, is [unverified]. | Check this in S2. |
| Fleet redactor gaps | — | `github_pat_`, `ghs_`/`ghu_`/`ghr_`, URL userinfo. | AFT module `Redactor`, reusing the patterns already in `packages/aft-cli/src/lib/sanitize.ts`. Also propose these upstream to commons. |
| TS plugin logger (`opencode-plugin/src/logger.ts`, same in pi) | all | Writes `DEBUG` with **no level filter** and appends `JSON.stringify(data)`. bash completion wakes log `reminder_sha256`/`reminder_chars`, not the reminder text (`bg-notifications.ts:563`) [src]. No `log(... JSON.stringify(params|args))` site found. | Under the TS twin, `CK_LOG` filtering applies and data fields render as `k=v`. |
| Panic hook | ERROR | Panic payload plus a forced backtrace for **every** panic, including panics AFT catches in dispatch (`dispatch_panic_response`). | Acceptable. Note the new volume. |

## 5. The plugin log (`aft-plugin.log`): in scope

It is in scope, for three reasons.

- **The spec says so.** "Every module and every plugin lane". For AFT and MC
  specifically: "the old `$TMPDIR` plugin log and pid-suffixed file are
  read-only compatibility inputs, never written again after the cut."
- **Technical reason.** `RotatingLogSink` (`durable-log.ts:82-94`) rotates by
  `rename`. Several harness processes (OpenCode instances, Pi) append to the
  same `aft-plugin.log`, and nothing coordinates them across processes. A
  writer that still holds the old path keeps writing into `.1`, which is the
  r1 multi-writer race that r2's never-renamed segment exists to remove. The
  plugin also creates the file with default mode (`0644` [obs]).
- **Duplication.** In standalone mode, every Rust line is written twice today:
  once to `aft-<pid>.log` and once relayed into `aft-plugin.log`. With one
  segment, the relay must stop persisting.

Implementation: `@cortexkit/log` 0.2.0, `forPlugin("aft", harness)`, which
writes to the same `aft.<date>.log`. Caveat: the twin opens with
`fs.openSync(…, "a", 0o600)` and writes with `fs.writeSync` (index.ts:711), synchronously on the host's event loop.
Today the plugin uses an async promise queue. The latency cost inside
OpenCode/Pi has not been measured [unverified]. Test runs route to
`aft-plugin-test.log` today; after the move they should use a temporary
`AFT_STORAGE_DIR` instead.

## 6. Consumers that break

| Consumer | Depends on | Must change |
|---|---|---|
| Health sentinel `scripts/sentinel/aft-health-sentinel.ts` | `daemonPidLog` (`:826-837`) finds the daemon by the file name `^aft-\d+\.log$` plus `ps --subc`. `readNew` has a per-path byte cursor. `log_bytes_added` counts the whole file's growth. `instrument:log-silent`. `CLI_LOG_LINE` (`:135`) strips `[aft]`-tagged CLI stderr from instrument errors. Plugin wake failures come from `logs/aft-plugin.log` (`:1116`). `rootFrom` (`:185`) is an unanchored `root=` regex. | Read `aft.<UTC date>.log` and keep lines whose bracket has `pid=<daemon pid>`. At the UTC roll, finish the previous segment from its cursor before moving on, so the tail of the old file is not lost. Compute growth from the filtered lines, not file size, because the file now has several writers. Take plugin lines from the same segment with `harness=`. Update `CLI_LOG_LINE` to the r2 shape. `rootFrom` has to handle a `root=` inside the bracket and possibly quoted. `LOG_LINE_TS` already accepts millis. Message-anchored detectors (panic, limiter, `route.bind`, `sandbox setup`, `perf tier2 phases`, `slow tool_call`, `bash_completion_wake_*`) survive. Add **mixed r1/r2 file** fixtures next to `fixtures/healthy-limiter.log` and the wedge-specimen replay. Update `docs/ops/aft-health-sentinel.md`. |
| `aft doctor --issue` (`packages/aft-cli/src/commands/doctor.ts:1563-1599`) | Reads **only** `adapter.getLogFile()`, which is `aft-plugin.log` (`adapters/opencode.ts:436`, `pi.ts:293`, `omp.ts:206`). It tails 200 and 4000 lines. It never reads `aft-<pid>.log`, so in subc mode daemon errors are missing from issues today. | Read today's and yesterday's UTC segment (just after midnight UTC today's is nearly empty), plus the legacy files, for ≥1 release. **`filterLogToSession`** (`lib/issue-body.ts:40-54`) keeps every line that has no `[ses_…]`/`[uuid]` tag. An r2 line carries `session=opencode:ses_…` instead, so it counts as untagged and **every session's lines would go into a public issue**. The pattern must accept the bracket form in the same release. `extractRecentErrors` is keyword-based and survives. |
| `lib/bridge-tool-failures.ts` (issue "tool failures" section) | `aft-plugin.log` path. `STRUCTURED_CODE_PATTERN` `"code":"…"` matches the JSON data tail. The `[aft-plugin]`/`[aft]` tag check (`:106`). `SESSION_TAG_PATTERN`. | Match `code=` fields, key on the ` ERROR ` level column plus the `aft` logger, and strip `session=`. |
| `scripts/telemetry/index-census.py`, `scripts/telemetry/oss-matrix.py` | `glob("aft-*.log")`. The timestamp regexes `^…T\d{2}:\d{2}:\d{2}Z` **do not allow millis**, so every r2 line would silently fail to match. `SESSION_RE` `\[ses_…\]`. | Glob both names, accept millis, accept `session=`. Add a mixed-file self-test. |
| TS bridge crash/diagnostic text | "Check logs: <path>" (`bridge.ts:900`, `getLogFilePathVia`) | Point at the segment. |
| E2E harnesses | `tests/docker/test-e2e.sh:43-51` (`PLUGIN_LOG`), `tests/docker/opencode2/harness/isolation.ts:191`, `bash.scenario.test.ts:53,245` (message text) | New path. |
| Docs | `docs/config.md` §"Durable logs" (already stale: it says 20 MB × 5 and 7 days, while the code is 32 MB × 1 and 24 h), `packages/pi-plugin/README.md:145`, `docs/ops/aft-health-sentinel.md` | Rewrite. |
| Write census | `aft profile --writes` `Domain::Logs` | See §3. |
| logging.rs unit tests (rotation, dead-PID, recycled PID, budget, forensic keep, relic reap) | — | Retire them with the code; the legacy-sweep tests stay until the sweep goes. |
| "Drain census" | Not found as a reader in `scripts/sentinel/`. The drain lines (`runtime_drain.rs:409`, `subc/drain.rs:365-367`) are message-anchored. | No reader-specific change found [unverified that none exists outside this repo]. |
| Fleet readers (`ck module logs`, `fleet-pulse`) | Outside this repo | Will read AFT once AFT is on r2. |

## 7. Slice plan

Each slice can land on its own. Order matters where noted.

**S0: prerequisite, commons (not AFT).** Publish `cortexkit-log` to crates.io.
AFT publishes `agent-file-tools` to crates.io (`.github/workflows/release.yml:115`),
and crates.io rejects git dependencies. Useful additions to raise with commons
at the same time: a public layer constructor or a stderr-copy option;
redactor patterns for `github_pat_`/`ghs_` and URL userinfo; an emit API that
carries both a timestamp and bound fields, for off-thread writers. AFT is
blocked on the publish only.

**S1: readers learn both formats. Ships no later than the adoption release (the spec requires this).**
Covers the sentinel, doctor (`--issue`, `filterLogToSession`,
`bridge-tool-failures`), the telemetry scripts, and the adapters'
`getLogFile`, which becomes a list. Tests: mixed r1/r2 fixture files for each
reader, including a test that a `session=` line from another session is
**excluded** from an issue body. Also a UTC-roll test for the sentinel cursor,
and a pid-filter test with two pids in one segment.

**S2: content hardening. Independent, can land first.**
Remove `input` from the `main.rs:469` parse-error line. Add a pure
`aft_redactor(&str) -> Cow<str>` covering GitHub token variants, URL userinfo
and token query params, with unit tests. Truncate LSP `params` in the debug
lines. Resolve the LSP stderr-tail question from §4. Test: a malformed
request that carries a secret-shaped `command` leaves no trace in the log
output.

**S3: Rust adoption. Needs S0; release it together with S1 or after it.**
- Add `cortexkit-log` with `default-features = false`.
- Add the AFT `log::Log` bridge described in §3: fixed logger names,
  `session_span`, bound `pid`/`role`/`harness`, write-ledger credit, and the
  module redactor from S2.
- Per-role init: daemon, bridge and warmup log durably; `profile` goes to
  stderr only.
- No stderr tee in subc mode.
- Read `CK_LOG_MAX_AGE_DAYS`/`CK_LOG_ALARM_SEGMENT_MB` and `aft.jsonc log.*`,
  with `RUST_LOG` as a fallback for one release.
- Resolve the storage root before init.
- Delete `RotatingFile`, `sweep_logs` and `sync_storage_root`. Keep a
  legacy-only sweep for `aft-<pid>.log*` and `aft-plugin.log.N`.

Gate: a latency benchmark of synchronous writes on the executor thread
against the current queue. If it regresses, keep an AFT-side queue and take
the timestamp/span loss, or wait for the S0 API.

Tests:
- A rendered line round-trips through `cortexkit_log::parse_line` and matches
  the fleet golden fixture.
- **No `log.target=` fields appear.**
- `session=` is present inside a request.
- Two child processes each append N lines to one segment; all 2N lines parse
  and carry their own `pid=`.
- The file is `0600` and the directory `0700`.
- `aft profile` creates no file.
- In subc mode, normal lines are absent from stderr.
- Legacy sweep: removes dead `aft-<pid>.log` and never touches
  `aft.<date>.log`.

**S4: plugin adoption. Needs S1; may ship with S3 or in the release after it.**
`@cortexkit/log` `forPlugin("aft", harness)` in the opencode and pi loggers.
The bridge keeps its stderr tail but stops persisting relayed Rust lines.
Remove `RotatingLogSink`; `aft-plugin.log` becomes a read-only input.
Tests:
- Plugin lines parse as r2 and carry `harness=`.
- One Rust line reaches the segment exactly once.
- `data` renders as `k=v`.
- Test runs write under a temporary `AFT_STORAGE_DIR`.

**S5: cleanup, at least one release after S3/S4 and more than 14 days later.**
Drop the legacy-format read paths from readers. Remove the legacy sweep.
Rewrite `docs/config.md`, the pi README and the sentinel ops doc. Name `pid=`
and `role=` in AFT's fleet census row.

## Open decisions

1. Retention: age-only 14 days, or a shorter AFT `max_age_days`, now that the
   200 MB budget has no equivalent.
2. Off-thread writes: measure first (S3 gate). If needed, wait for a crate API.
3. Session issuer source (`opencode`/`pi`/…) inside the Rust process, and what
   to do with the "last session on this thread" fallback.
4. Whether a standalone bridge keeps any stderr copy beyond panics.
