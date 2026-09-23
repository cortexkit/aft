#!/usr/bin/env bun
/** Short-lived AFT health sentinel. Collection is impure; every detector below is pure. */
import { SubcClient } from "@cortexkit/subc-client";
import { appendFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, statSync, truncateSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
import { spawnSync } from "node:child_process";

export type Severity = "CRITICAL" | "WARNING";
export type Finding = { severity: Severity; rule: string; fingerprint: string; text: string; clears_when: string };
export type Plane = {
  status?: string;
  reason?: string;
  since_ms?: number | null;
  next_retry_ms?: number | null;
  last_progress_at_ms?: number | null;
  stale_since_ms?: number | null;
  next_refresh_at_ms?: number | null;
  pending_paths?: number | null;
};
export type RootHealth = {
  project_root?: string;
  search_index?: Plane;
  semantic_index?: Plane;
  tier2?: Plane;
  watcher?: Record<string, unknown>;
  bound_routes?: number;
  idle_ms?: number;
  root_ttl_ms?: number;
};
export type LimiterEntry = { domain?: string; root?: string; kind?: string; acquired_at_ms?: number; age_ms?: number };
/** One completed or in-flight GitHub Actions run, as `gh run list --json` reports it. */
export type ScheduledRun = {
  workflow?: string;
  branch?: string;
  event?: string;
  status?: string;
  conclusion?: string | null;
  created_at?: string;
  run_id?: number;
};
export type SentinelSample = {
  now_ms: number;
  supervisor?: { running: boolean; pid?: number; last_exit_code?: number | null; placement_recorded?: boolean };
  health?: Record<string, any>;
  health_error?: string;
  memory_census?: Record<string, any>;
  memory_error?: string;
  writes_census?: Record<string, any>;
  writes_error?: string;
  log_lines?: string[];
  /** Bytes the pid log gained since the previous run; distinguishes an idle daemon from an unreadable log. */
  log_bytes_added?: number;
  log_error?: string;
  plugin_lines?: string[];
  plugin_error?: string;
  process?: { pid?: number; phys_footprint_bytes?: number; cpu_percent?: number; bytes_written?: number; image?: string };
  process_error?: string;
  /**
   * free_bytes is what `df` reports, which excludes purgeable space.
   * available_bytes is the purgeable-inclusive capacity (see
   * availableForImportantUsage); available_error says why it is absent.
   */
  disk?: { free_bytes?: number; available_bytes?: number; available_error?: string; sizes?: Record<string, number>; artifact_sizes?: Record<string, number>; artifact_roots?: Record<string, string> };
  disk_error?: string;
  dsym?: { requested_uuid?: string; found_uuid?: string; path?: string; unreadable?: boolean; error?: string };
  /** Newest-first scheduled workflow runs on the main branch; see detectScheduledCi. */
  ci_runs?: ScheduledRun[];
  /**
   * Workflows whose own targeted listing could not be fetched, so their rows in
   * ci_runs come from the combined listing. The combined listing has served
   * stale per-workflow rows, so detectScheduledCi refuses a verdict for these.
   */
  ci_fallback?: string[];
  ci_error?: string;
};
export type FindingLedger = Record<string, { last_alerted_at: number; last_seen_at: number; severity: Severity; rule: string; text: string; cleared_at?: number }>;
/** One limiter-relevant log event: a cold-build deferral or a slot acquisition. */
export type LimiterEvent = { ts_ms: number; kind: "deferred" | "acquired"; line?: string };
export type LimiterWindow = { pid?: number; events: LimiterEvent[]; truncated?: boolean };
export type SentinelState = {
  findings: FindingLedger;
  log?: { path?: string; offset?: number; size?: number };
  plugin_log?: { path?: string; offset?: number; size?: number };
  previous?: { pid?: number; free_bytes?: number; available_bytes?: number; sampled_at_ms?: number; bytes_written?: number; unexplained_write_rate_runs?: number; sizes?: Record<string, number>; artifact_sizes?: Record<string, number>; watcher?: Record<string, [number, number]>; tool_slow_roots?: string[] };
  /** Last scheduled-run listing and when it was fetched, so the poll can be slower than the tick. */
  ci?: { checked_at_ms?: number; runs?: ScheduledRun[]; fallback?: string[]; error?: string };
  /** Consecutive ticks each instrument fingerprint has failed; see gateInstrumentFindings. */
  instrument_failures?: Record<string, number>;
  /** Rolling 15-minute window of limiter events, maintained across ticks by updateLimiterWindow. */
  limiter?: LimiterWindow;
};

const HOME = homedir();
const SHARE = join(HOME, ".local", "share", "cortexkit");
const AFT = join(SHARE, "aft");
export const STATE_DIR = process.env.AFT_SENTINEL_STATE_DIR
  ?? (process.env.NODE_ENV === "test"
    ? mkdtempSync(join(tmpdir(), "aft-health-sentinel-test-"))
    : join(HOME, ".local", "state", "cortexkit", "aft", "sentinel-dev"));
export const STATE_FILE = join(STATE_DIR, "state.json");
export const FINDINGS_FILE = join(STATE_DIR, "findings.jsonl");
const CONNECTION = join(SHARE, "run", "subc-connection.json");
const CRITICAL_COOLDOWN = 30 * 60_000;
const WARNING_COOLDOWN = 2 * 60 * 60_000;
const TEN_MINUTES = 10 * 60_000;
const FIFTEEN_MINUTES = 15 * 60_000;
const TIER2_REFRESH_OVERDUE_GRACE = 5 * 60_000;
const GB = 1024 ** 3;

const MAX_LAUNCHD_LOG_BYTES = 1024 * 1024;
const LAUNCHD_LOGS = [join(STATE_DIR, "launchd.stdout.log"), join(STATE_DIR, "launchd.stderr.log")];

export function capLaunchdLogs(paths = LAUNCHD_LOGS): void {
  for (const path of paths) {
    try {
      if (statSync(path).size > MAX_LAUNCHD_LOG_BYTES) truncateSync(path, 0);
    } catch {
      // launchd creates these files before the process starts; a missing path is harmless.
    }
  }
}

function finding(rule: string, severity: Severity, fingerprint: string, text: string, clears_when: string): Finding {
  return { rule, severity, fingerprint, text, clears_when };
}
function instrument(name: string, detail: string): Finding {
  return finding("instrument", "WARNING", `instrument:${name}`, `instrument ${name} unavailable: ${instrumentErrorText(detail)}`, "the input is readable again");
}
// The aft CLI writes its own log lines to stderr (for example
// "[aft] log retention sweep: ..." or "[aft] login-shell PATH probe: ...",
// optionally behind a UTC timestamp). A failing command's stderr therefore
// carries those lines ahead of the line that explains the failure, and
// String(error) prefixes the first one with "Error: ".
const CLI_LOG_LINE = /^(?:Error:\s*)?(?:\d{4}-\d\d-\d\dT\S+\s+)?\[aft(?:-lsp)?\]\s/;
/** Drop CLI log lines from an instrument error, keeping the lines that explain the failure. */
export function instrumentErrorText(detail: string): string {
  const kept = detail.split("\n").filter((line) => line.trim() !== "" && !CLI_LOG_LINE.test(line));
  // If every line was a log line, the original text is still better than nothing.
  return kept.length > 0 ? kept.join("\n").trim() : detail.trim();
}
// Under heavy load (load average 250-385 on 2026-09-22) the ps and gh spawns
// time out and one tick loses an input for reasons unrelated to the input
// itself. An instrument warning is raised only when the same instrument has
// failed on this many consecutive ticks; one success resets the count.
export const INSTRUMENT_FAILURE_STREAK = 3;
/**
 * Hold back instrument findings until their instrument has failed on
 * INSTRUMENT_FAILURE_STREAK consecutive ticks.
 *
 * `prior` is the per-fingerprint count persisted from the previous tick. The
 * returned `streaks` holds only instruments failing this tick, so an
 * instrument that succeeded drops out and restarts from zero. Every tick is a
 * fresh process, which is why the count lives in the state file.
 */
export function gateInstrumentFindings(
  findings: Finding[],
  prior: Record<string, number> = {},
): { findings: Finding[]; streaks: Record<string, number> } {
  const streaks: Record<string, number> = {};
  const kept: Finding[] = [];
  for (const value of findings) {
    if (value.rule !== "instrument") {
      kept.push(value);
      continue;
    }
    const count = (prior[value.fingerprint] ?? 0) + 1;
    streaks[value.fingerprint] = count;
    if (count >= INSTRUMENT_FAILURE_STREAK) kept.push({ ...value, text: `${value.text} (failed ${count} consecutive ticks)` });
  }
  return { findings: kept, streaks };
}
function metrics(sample: SentinelSample): Record<string, any> { return sample.health?.metrics ?? sample.health?.health?.metrics ?? {}; }
export function healthBytesWritten(sample: SentinelSample): { available: boolean; bytes?: number } {
  const processIo = metrics(sample).process_io;
  if (!processIo || processIo.available === false) return { available: false };
  const bytes = processIo.diskio_bytes_written ?? processIo.logical_bytes_written;
  return typeof bytes === "number" && Number.isFinite(bytes)
    ? { available: true, bytes }
    : { available: true };
}

function roots(sample: SentinelSample): RootHealth[] { return Array.isArray(metrics(sample).roots) ? metrics(sample).roots : []; }
function rootFrom(line: string): string {
  return line.match(/\broot=(.+?)(?:\s+(?:key|category|session|kind|outcome|files|ms)=|$)/)?.[1]
    ?? line.match(/route\.bind within[^:]*:\s*(\/[^ ]+)/)?.[1]
    ?? "unknown";
}

const LOG_LINE_TS = /^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z)/;
// The daemon logs deferrals as "... deferred by cold build limit ..." and
// acquisitions in two shapes: "<class> cold-build slot acquired after Nms
// wait: request=... kind=..." (class is inspect-triggered, maintenance, or
// standing) and "maintenance build slot acquired after Nms wait: ..." — see
// acquire_or_wait in crates/aft/src/cold_build_limiter.rs.
const LIMITER_DEFERRED_LINE = /deferred by cold build limit/i;
const LIMITER_ACQUIRED_LINE = /cold-build slot acquired|maintenance build slot acquired/i;
// The state file is rewritten on every tick, so the window is capped: a log
// that floods deferrals must not grow it without limit. When the cap is hit
// the oldest entries are dropped and `truncated` is set, meaning the window
// now covers less than the full 15 minutes rather than the state file
// growing past this bound.
export const LIMITER_EVENT_LIMIT = 2000;

/**
 * Roll the persisted limiter evidence window forward by one tick.
 *
 * `lines` carries only what the daemon appended since the previous tick (the
 * log cursor in the state file), so the 15-minute window detectLimiter
 * reports on cannot be computed from the sample alone; it has to live in the
 * state file. Each tick appends the timestamps of the new deferral and
 * acquisition lines and prunes entries older than the window. A pid change
 * means a new daemon process, and the old process's events say nothing about
 * its limiter, so the buffer starts empty. Both pids must be known for that
 * verdict: a state file from before this buffer existed has no recorded pid,
 * and that absence is not a change.
 */
export function updateLimiterWindow(prior: LimiterWindow | undefined, lines: string[], nowMs: number, pid?: number): LimiterWindow {
  const restarted = prior?.pid !== undefined && pid !== undefined && prior.pid !== pid;
  const events: LimiterEvent[] = restarted ? [] : [...(prior?.events ?? [])];
  for (const line of lines) {
    const kind = LIMITER_DEFERRED_LINE.test(line) ? "deferred" : LIMITER_ACQUIRED_LINE.test(line) ? "acquired" : undefined;
    if (!kind) continue;
    const parsed = Date.parse(line.match(LOG_LINE_TS)?.[1] ?? "");
    // An undated line still happened on this tick; the tick's own time stands
    // in for the missing timestamp.
    const ts_ms = Number.isFinite(parsed) ? parsed : nowMs;
    // The acquisition text is kept for the holder fallback in detectLimiter;
    // deferral text carries nothing the count does not.
    events.push(kind === "acquired" ? { ts_ms, kind, line: line.trim() } : { ts_ms, kind });
  }
  const kept = events.filter((event) => nowMs - event.ts_ms <= FIFTEEN_MINUTES);
  const overflow = kept.length - LIMITER_EVENT_LIMIT;
  return { pid, events: overflow > 0 ? kept.slice(overflow) : kept, ...(overflow > 0 ? { truncated: true } : {}) };
}

export function detectDaemon(sample: SentinelSample, state: SentinelState): Finding[] {
  const out: Finding[] = [];
  if (!sample.supervisor) out.push(instrument("supervisor", "module status was not read"));
  if (sample.health_error) out.push(instrument("health-check", sample.health_error));
  // daemon.down pages only on the supervisor's own word: it reports aft not
  // running, or it could not be read at all (the collector sets running=false
  // only after its confirming read failed too). A failed health probe with a
  // live supervisor behind it is an instrument failure, which the
  // consecutive-tick gate handles: on 2026-09-23 at load 255 one timed-out
  // probe ("Error: exit null") paged CRITICAL here while the supervisor read
  // running/healthy with zero restarts.
  if (sample.supervisor?.running === false) {
    out.push(finding("daemon.down", "CRITICAL", "daemon:aft", `AFT daemon is unavailable (${sample.health_error ?? "supervisor reports not running"})`, "health.check is reachable and the supervisor reports aft running"));
  }
  const oldPid = state.previous?.pid;
  const newPid = sample.supervisor?.pid ?? sample.process?.pid;
  if (oldPid && newPid && oldPid !== newPid && !sample.supervisor?.placement_recorded) {
    out.push(finding("daemon.restarted", "WARNING", "daemon:aft:restart", `AFT pid changed ${oldPid} -> ${newPid}; last exit code ${sample.supervisor?.last_exit_code ?? "unknown"}`, "the next run observes the same pid"));
  }
  return out;
}

export function detectLogHealth(sample: SentinelSample): Finding[] {
  if (sample.log_error) return [instrument("daemon-log", sample.log_error)];
  const lines = sample.log_lines ?? [];
  const out: Finding[] = [];
  // A silent log only means the instrument is broken if there was something to
  // read. An idle daemon legitimately writes nothing -- at night, with no
  // session bound and no maintenance due, silence is the correct state, and
  // warning about it teaches the reader to skip the channel. Bytes appended
  // with no lines parsed is the real failure: we are reading the wrong file,
  // or the cursor is wrong.
  if (sample.supervisor?.running && sample.health && lines.length === 0 && Number(sample.log_bytes_added ?? 0) > 0) {
    out.push(instrument("log-silent", `daemon log grew by ${sample.log_bytes_added} bytes but no lines could be read from it`));
  }
  for (const line of lines) {
    if (/panicked at|actor_fatal|fatal executor/i.test(line)) {
      out.push(finding("daemon.panic", "CRITICAL", `panic:${sample.supervisor?.pid ?? "aft"}`, line.trim(), "a subsequent log window contains no panic signature"));
    }
    if (/did not answer route\.bind within/i.test(line)) {
      const root = rootFrom(line);
      out.push(finding("bind.stall", "CRITICAL", `bind:${root}`, line.trim(), "a subsequent window contains no route.bind timeout for this root"));
    }
    if (/sandbox setup for .* failed/i.test(line)) {
      const cause = line.match(/sandbox setup for (.* failed.*)$/i)?.[1] ?? line;
      out.push(finding("sandbox.refusal", "WARNING", `sandbox:${rootFrom(line)}`, cause.trim(), "a subsequent window contains no sandbox setup refusal for this root"));
    }
  }
  return out;
}

export function detectLimiter(sample: SentinelSample, state: SentinelState): Finding[] {
  if (sample.log_error) return [instrument("limiter-log", sample.log_error)];
  // The window is the persisted buffer updateLimiterWindow maintains across
  // ticks, not this tick's log lines: sample.log_lines covers only what the
  // daemon appended since the previous tick, so judging "in 15m" from it
  // pages on every restart storm that defers right after acquiring.
  const events = (state.limiter?.events ?? []).filter((event) => sample.now_ms - event.ts_ms <= FIFTEEN_MINUTES);
  const deferred = events.filter((event) => event.kind === "deferred");
  const acquired = events.filter((event) => event.kind === "acquired");
  if (deferred.length < 5 || acquired.length > 0) return [];
  const limiter = metrics(sample).cold_build_limiter;
  const holders: LimiterEntry[] = limiter?.holders ?? [];
  const holderText =
    acquired.slice(-2).map((event) => event.line ?? "").filter(Boolean).reverse().join(" | ")
    || holders.map((h) => `${h.kind ?? h.domain ?? "build"}@${h.root ?? "unknown"} age=${h.age_ms ?? "?"}ms`).join(", ")
    || "holders unavailable";
  return [finding("limiter.saturated", "CRITICAL", "limiter:cold-build", `${deferred.length} cold-build deferrals with no acquisition in 15m; holders: ${holderText}`, "a slot is acquired or fewer than five deferrals occur in the 15-minute window")];
}

export function detectRetention(sample: SentinelSample): Finding[] {
  const retention = metrics(sample).bash_task_retention;
  const eligible = retention?.eligible_but_unpruned_rows;
  const steadyCeiling = retention?.steady_state_ceiling;
  if (!Number.isFinite(eligible) || !Number.isFinite(steadyCeiling) || eligible <= steadyCeiling) return [];
  return [finding(
    "retention.backlog",
    "WARNING",
    "retention:bash-tasks",
    `${eligible} bash task rows are eligible but unpruned; steady-state capacity is ${steadyCeiling} rows per tick`,
    `eligible bash task rows fall to ${steadyCeiling} or fewer`,
  )];
}

export function detectIndexes(sample: SentinelSample): Finding[] {
  const out: Finding[] = [];
  const healthMetrics = metrics(sample);
  const backend = healthMetrics.embedding_backend;
  if (backend?.available === false) {
    const affectedRoots = roots(sample)
      .filter((root) => root.semantic_index?.status === "backend_unavailable")
      .map((root) => root.project_root ?? "unknown");
    const rootText = affectedRoots.length > 0 ? affectedRoots.join(", ") : "affected roots omitted from metrics";
    const reason = typeof backend.last_error === "string" ? backend.last_error : "embedding backend unavailable";
    out.push(finding(
      "embedding.backend_down",
      "WARNING",
      "embedding:backend",
      `embedding backend unavailable for ${rootText}: ${reason}`,
      "the embedding backend answers successfully",
    ));
  }
  for (const root of roots(sample)) {
    const rootPath = root.project_root ?? "unknown";
    const tier2 = root.tier2;
    if (
      tier2?.status === "stale"
      && typeof tier2.next_refresh_at_ms === "number"
      && sample.now_ms > tier2.next_refresh_at_ms + TIER2_REFRESH_OVERDUE_GRACE
    ) {
      const pendingPaths = typeof tier2.pending_paths === "number" ? tier2.pending_paths : 0;
      const pathLabel = pendingPaths === 1 ? "path" : "paths";
      const overdueMinutes = Math.round((sample.now_ms - tier2.next_refresh_at_ms) / 60_000);
      out.push(finding(
        "tier2.refresh_overdue",
        "WARNING",
        `tier2-refresh:${rootPath}`,
        `${rootPath} tier2 refresh missed its scheduler deadline by ${overdueMinutes}m with ${pendingPaths} pending ${pathLabel}`,
        "tier2 reports ready or building",
      ));
    }
    for (const plane of ["search_index", "semantic_index", "tier2"] as const) {
      const status = root[plane];
      if (status?.status === "backend_unavailable") continue;
      if (status?.status !== "building") continue;
      const since = status.since_ms;
      const progress = status.last_progress_at_ms;
      if (typeof since !== "number") {
        out.push(instrument("index-timestamps", `${rootPath} ${plane} is building without since_ms`));
        continue;
      }
      const lastMotion = typeof progress === "number" ? Math.max(since, progress) : since;
      if (sample.now_ms - since > TEN_MINUTES && sample.now_ms - lastMotion > TEN_MINUTES) {
        const label = plane.replace("_index", "");
        out.push(finding("index.stuck", "CRITICAL", `index:${rootPath}:${label}`, `${rootPath} ${label} has been building for ${Math.round((sample.now_ms - since) / 60_000)}m without progress`, "the plane reports ready or its progress timestamp advances"));
      }
    }
  }
  return out;
}

export function detectTier2Overlong(sample: SentinelSample): Finding[] {
  if (sample.log_error) return [instrument("tier2-log", sample.log_error)];
  const lines = sample.log_lines ?? [];
  const completions = new Set(lines.filter((line) => /perf tier2 phases/.test(line)).map((line) => `${rootFrom(line)}:${line.match(/category=([^ ]+)/)?.[1] ?? "unknown"}`));
  const out: Finding[] = [];
  for (const line of lines) {
    const match = line.match(/^(\S+).*cold-build slot acquired.*request=inspect:(.+?):\d+.*Tier-2 run/i);
    if (!match) continue;
    const started = Date.parse(match[1]);
    const root = match[2];
    if (Number.isFinite(started) && sample.now_ms - started > TEN_MINUTES && ![...completions].some((key) => key.startsWith(`${root}:`))) {
      out.push(finding("tier2.pass_overlong", "WARNING", `tier2:${root}`, `Tier-2 slot for ${root} has no perf tier2 phases completion after ${Math.round((sample.now_ms - started) / 60_000)}m`, "a matching perf tier2 phases line appears or the pass is cancelled"));
    }
  }
  return out;
}

function executorHealth(sample: SentinelSample): {
  source: "dispatch_liveness" | "legacy" | "missing";
  zombie: number;
  inflight: number;
  workersIdle: boolean;
  queueAgeMs?: number;
} {
  const all = metrics(sample);
  const dispatch = all.dispatch_liveness;
  if (dispatch) {
    return {
      source: "dispatch_liveness",
      zombie: Number(dispatch.executor_zombie_reader ?? dispatch.running?.phantom?.maintenance ?? 0),
      inflight: Number(dispatch.maintenance_inflight ?? dispatch.running?.maintenance ?? 0),
      workersIdle: Number(dispatch.running?.interactive ?? 0) === 0 && Number(dispatch.running?.maintenance ?? 0) === 0,
      queueAgeMs: dispatch.maintenance?.oldest_age_ms ?? undefined,
    };
  }
  const legacy = all.executor ?? all;
  const inflight = legacy.maintenance_inflight ?? legacy.dispatch_path?.completion_channels?.maintenance;
  const queueAgeMs = legacy.maintenance_queue_oldest_age_ms ?? legacy.maintenance_oldest_age_ms;
  if (inflight !== undefined || queueAgeMs !== undefined) {
    const runningMaintenance = Number(legacy.running_maintenance ?? legacy.maintenance_running ?? 0);
    return { source: "legacy", zombie: Number(legacy.executor_zombie_reader ?? 0), inflight: Number(inflight ?? 0), workersIdle: runningMaintenance === 0, queueAgeMs: queueAgeMs === undefined ? undefined : Number(queueAgeMs) };
  }
  return { source: "missing", zombie: 0, inflight: 0, workersIdle: true };
}

export function detectExecutor(sample: SentinelSample, state: SentinelState): Finding[] {
  const executor = executorHealth(sample);
  if (executor.source === "missing") {
    return [instrument("executor-health", "metrics.dispatch_liveness waits on the sentinel health card; legacy fallback fields maintenance_inflight and maintenance_queue_oldest_age_ms are also absent")];
  }
  const priorPhantom = state.previous?.sampled_at_ms && (state as any).previous?.phantom_inflight;
  if (executor.zombie > 0 || (executor.inflight > 0 && executor.workersIdle && priorPhantom)) {
    const source = executor.source === "legacy" ? "legacy fallback maintenance_inflight/queue age" : "dispatch_liveness";
    return [finding("executor.phantom", "CRITICAL", "executor:maintenance", `executor health (${source}) reports zombie_reader=${executor.zombie}, maintenance_inflight=${executor.inflight}, queue_age_ms=${executor.queueAgeMs ?? "unknown"} while workers are idle`, "zombie readers are zero and maintenance in-flight agrees with active workers")];
  }
  return [];
}

// A wake failure is one of the plugin's own failure events. Matching the
// words `promptAsync` or `this._client` anywhere was wrong twice over: the
// OpenCode host log records every bash permission decision with the command
// text, so an operator grepping for those words planted "failures", and a
// successful `bash_completion_wake_prompt_async_ok` line matched too.
export const WAKE_FAILURE_LINE = /bash_completion_wake_(prompt_async_error|client_unavailable|refire_error)/;

export function detectWakes(sample: SentinelSample): Finding[] {
  const runtime = metrics(sample).runtime ?? {};
  const oldest = Number(runtime.bg_wake_oldest_unacked_age_ms ?? 0);
  const failures = (sample.plugin_lines ?? []).filter((line) => WAKE_FAILURE_LINE.test(line));
  if (sample.plugin_error) return [instrument("plugin-log", sample.plugin_error)];
  if (oldest > 5 * 60_000 || failures.length) {
    return [finding("wakes.backlog", "CRITICAL", "wakes:opencode", `${failures.length} plugin wake failures; oldest unacked completion age=${oldest}ms`, "oldest unacked age is at most five minutes and no plugin delivery failure occurs in the window")];
  }
  return [];
}

export function detectWatcher(sample: SentinelSample, state: SentinelState): Finding[] {
  const out: Finding[] = [];
  for (const root of roots(sample)) {
    const watcher = root.watcher;
    if (!watcher) continue;
    const path = root.project_root ?? "unknown";
    const kernel = Number(watcher.rescans_kernel_dropped_total ?? 0);
    const user = Number(watcher.rescans_user_dropped_total ?? 0);
    const before = state.previous?.watcher?.[path] ?? [kernel, user];
    if (kernel > before[0] || user > before[1]) {
      out.push(finding("watcher.overflow", "WARNING", `watcher:${path}`, `${path} watcher rescans increased kernel=${kernel - before[0]} user=${user - before[1]}; prefixes=${JSON.stringify(watcher.last_overflow_prefixes ?? [])}`, "no dropped-event rescan counter increases in the next window"));
    }
  }
  return out;
}

export function detectStorage(sample: SentinelSample, state: SentinelState): Finding[] {
  if (sample.disk_error) return [instrument("disk", sample.disk_error)];
  const out: Finding[] = [];
  const free = sample.disk?.free_bytes;
  if (typeof free !== "number") return [instrument("disk", "free byte count is absent")];
  // Severity is keyed to the action it implies, not to a round number.
  // 25 GiB is where the operator's documented policy says to reclaim other
  // seats' images; below that, acting beats watching. Above it, a night of
  // normal mason traffic cycles the level by tens of gigabytes (34 -> 50 -> 39
  // GiB on 2026-09-22) and every trough recovered unaided, so paging CRITICAL
  // there trains the reader to skim the channel that has to work at 25.
  // The delta against the previous sample separates a build in flight from a
  // real leak, which the level alone cannot do.
  //
  // `df` free space excludes purgeable space (mostly local Time Machine
  // snapshots), which macOS releases by itself when the volume runs low. On
  // 2026-09-22 a df-based reading paged CRITICAL at 18 and 16 GiB minutes
  // before macOS freed the snapshots (206 and 177 GiB free right after). The
  // severity is therefore judged on the purgeable-inclusive capacity; df is
  // kept in the text so a reader sees both. When the purgeable-inclusive probe
  // failed, df is judged instead and the text says purgeable space is not
  // counted, so a probe failure never silently drops the rule.
  const available = sample.disk?.available_bytes;
  const judged = typeof available === "number" ? available : free;
  // Compare like with like: a purgeable-inclusive reading against the previous
  // purgeable-inclusive reading, a df reading against the previous df reading.
  const priorJudged = typeof available === "number" ? state.previous?.available_bytes : state.previous?.free_bytes;
  const trend =
    typeof priorJudged === "number" && Math.abs(judged - priorJudged) >= GB / 2
      ? `, ${judged < priorJudged ? "falling" : "rising"} ${(Math.abs(judged - priorJudged) / GB).toFixed(1)} GiB since the last sample`
      : "";
  const gib = (bytes: number) => `${(bytes / GB).toFixed(1)} GiB`;
  const freeText = typeof available === "number"
    ? `AFT data volume has ${gib(available)} available including purgeable space (df free: ${gib(free)})${trend}`
    : `AFT data volume has ${gib(free)} free by df; purgeable space is not counted (${sample.disk?.available_error ?? "purgeable-inclusive probe did not run"})${trend}`;
  if (judged < 25 * GB) out.push(finding("disk.low", "CRITICAL", "disk:aft-data", freeText, "available space reaches 25 GiB"));
  else if (judged < 50 * GB) out.push(finding("disk.low", "WARNING", "disk:aft-data", freeText, "available space reaches 50 GiB"));
  for (const [name, size] of Object.entries(sample.disk?.sizes ?? {})) {
    const prior = state.previous?.sizes?.[name];
    if (typeof prior === "number" && size - prior > 5 * GB) {
      out.push(finding("storage.growth", "WARNING", `storage:${name}`, `${name} grew by ${((size - prior) / GB).toFixed(1)} GiB in one interval`, "the next interval grows by at most 5 GiB"));
    }
  }
  return out;
}

export function writeGrowthAttribution(sample: SentinelSample, state: SentinelState, writeDelta: number): string {
  if (writeDelta <= 0) return "";
  const ledger = sample.writes_census?.writers ?? metrics(sample).write_ledger_top_10m;
  if (Array.isArray(ledger) && (ledger.length > 0 || sample.writes_census)) {
    // The ledger rows come from the census's own window (10 minutes), not the
    // sentinel's sampling interval, so shares are taken against that window's
    // process total; against the interval delta they summed past 100%.
    const census = sample.writes_census;
    const censusProcessBytes = Number(census?.process?.physical_bytes ?? 0);
    const windowTotal = censusProcessBytes > 0
      ? censusProcessBytes
      : ledger.reduce((sum: number, entry: Record<string, unknown>) => sum + Number(entry.physical_bytes ?? 0), 0);
    const windowMinutes = census?.since_ms && census?.until_ms
      ? Math.round((Number(census.until_ms) - Number(census.since_ms)) / 60_000)
      : 10;
    const lines = ledger.slice(0, 3).map((entry: Record<string, unknown>, index: number) => {
      const bytes = Number(entry.physical_bytes ?? 0);
      const share = windowTotal > 0 ? Math.min(100, bytes / windowTotal * 100) : 0;
      return `${index + 1}. ${(bytes / GB).toFixed(2)} GiB ${entry.domain ?? "other"} (${entry.root_id ?? "unknown"}); ${share.toFixed(0)}% of the ${windowMinutes}-minute window`;
    });
    const unmeasurable = Number(census?.unmeasurable_physical_bytes_estimate ?? 0);
    const seamNames = Array.isArray(census?.unmeasurable)
      ? census.unmeasurable.map((entry: Record<string, unknown>) => String(entry.seam ?? "unknown")).join(", ")
      : "";
    if (unmeasurable > 0 || seamNames) {
      const share = windowTotal > 0 ? `; ${Math.min(100, unmeasurable / windowTotal * 100).toFixed(0)}% of the ${windowMinutes}-minute window` : "";
      lines.push(`unmeasurable (estimate): ${(unmeasurable / GB).toFixed(2)} GiB${share}${seamNames ? `; seams: ${seamNames}` : ""}`);
    }
    const unexplained = Number(census?.unexplained_physical_bytes ?? 0);
    if (unexplained > 0) {
      const share = windowTotal > 0 ? `; ${Math.min(100, unexplained / windowTotal * 100).toFixed(0)}% of the ${windowMinutes}-minute window` : "";
      lines.push(`unexplained: ${(unexplained / GB).toFixed(2)} GiB${share}`);
    }
    return lines.length > 0 ? `\n${lines.join("\n")}` : "";
  }
  const current = sample.disk?.artifact_sizes ?? {};
  const previous = state.previous?.artifact_sizes ?? {};
  const growers = Object.entries(current)
    .map(([subject, size]) => ({ subject, bytes: Math.max(0, size - (previous[subject] ?? size)) }))
    .filter((entry) => entry.bytes > 0)
    .sort((left, right) => right.bytes - left.bytes || left.subject.localeCompare(right.subject))
    .slice(0, 3);
  const explained = growers.reduce((sum, entry) => sum + entry.bytes, 0);
  const lines = growers.map((entry, index) => {
    const share = Math.min(100, entry.bytes / writeDelta * 100);
    const root = sample.disk?.artifact_roots?.[entry.subject] ?? "unmapped";
    return `${index + 1}. ${(entry.bytes / GB).toFixed(2)} GiB ${entry.subject} (${root}) grew; ${share.toFixed(0)}% of write delta`;
  });
  if (explained < writeDelta / 2) lines.push("remainder: in-place rewrites (WAL churn)");
  return lines.length > 0 ? `\n${lines.join("\n")}` : "\nremainder: in-place rewrites (WAL churn)";
}

const UNEXPLAINED_WRITE_RATE_CEILING = GB;
// The 1 GiB/h ceiling now applies only to bytes left unexplained after both
// attributed writers and known-by-construction seam estimates are removed.
// One hot window can still be normal, so the alert remains sustained-only.
const WRITE_RATE_SUSTAINED_RUNS = 2;

function unexplainedWriteRate(sample: SentinelSample): number | undefined {
  const census = sample.writes_census;
  const unexplained = census?.unexplained_physical_bytes;
  const since = census?.since_ms;
  const until = census?.until_ms;
  if (typeof unexplained !== "number" || !Number.isFinite(unexplained)
    || typeof since !== "number" || typeof until !== "number" || until <= since) return undefined;
  const hours = Math.max(1 / 3600, (until - since) / 3_600_000);
  return Math.max(0, unexplained) / hours;
}

/** Consecutive census windows above the unexplained-write ceiling. */
function writeRateRunCount(sample: SentinelSample, previous: SentinelState["previous"]): number {
  const rate = unexplainedWriteRate(sample);
  if (rate === undefined) return 0;
  return rate > UNEXPLAINED_WRITE_RATE_CEILING
    ? (previous?.unexplained_write_rate_runs ?? 0) + 1
    : 0;
}

export function detectProcess(sample: SentinelSample, state: SentinelState): Finding[] {
  if (sample.process_error) return [instrument("process", sample.process_error)];
  const out: Finding[] = [];
  const proc = sample.process ?? {};
  if (Number(proc.phys_footprint_bytes ?? 0) > 6 * GB) out.push(finding("process.footprint", "WARNING", `process:${proc.pid ?? "aft"}:footprint`, `AFT physical footprint is ${(Number(proc.phys_footprint_bytes) / GB).toFixed(1)} GiB`, "physical footprint is at most 6 GiB"));
  if (Number(proc.cpu_percent ?? 0) > 150) out.push(finding("process.cpu", "WARNING", `process:${proc.pid ?? "aft"}:cpu`, `AFT CPU averaged ${proc.cpu_percent}% over the interval`, "interval CPU is at most 150%"));
  const rate = unexplainedWriteRate(sample);
  if (sample.writes_error) {
    out.push(instrument("writes-census", sample.writes_error));
  } else if (sample.writes_census && rate === undefined) {
    out.push(instrument("writes-census", "unexplained physical bytes or census window is unavailable"));
  }
  if (rate !== undefined) {
    const consecutive = writeRateRunCount(sample, state.previous);
    if (consecutive >= WRITE_RATE_SUSTAINED_RUNS) {
      const windowBytes = Number(sample.writes_census?.process?.physical_bytes ?? 0);
      const attribution = writeGrowthAttribution(sample, state, windowBytes);
      out.push(finding("process.writes", "WARNING", `process:${proc.pid ?? "aft"}:writes`, `AFT unexplained physical write rate is ${(rate / GB).toFixed(1)} GiB/h across ${consecutive} consecutive windows${attribution}`, "unexplained physical write rate is at most 1 GiB/h sustained"));
    }
  }
  return out;
}

const SLOW_TOOL_LINE = /slow tool_call .*\btotal=(\d+)ms/;
const TOOL_SLOW_CALL_MS = 10_000;
const TOOL_SLOW_BURST = 10;
/** Roots whose newest log lines hold more slow tool calls than tool.slow tolerates. */
function slowToolRoots(lines: string[]): string[] {
  const counts = new Map<string, number>();
  for (const line of lines) {
    if (Number(line.match(SLOW_TOOL_LINE)?.[1] ?? 0) <= TOOL_SLOW_CALL_MS) continue;
    const root = rootFrom(line);
    counts.set(root, (counts.get(root) ?? 0) + 1);
  }
  return [...counts].filter(([, count]) => count > TOOL_SLOW_BURST).map(([root]) => root).sort();
}

export function detectSearchAndTools(sample: SentinelSample, state?: SentinelState): Finding[] {
  if (sample.log_error) return [instrument("tool-log", sample.log_error)];
  const byRoot = new Map<string, { searches: number; degraded: number; slow: number }>();
  for (const line of sample.log_lines ?? []) {
    const root = rootFrom(line);
    const entry = byRoot.get(root) ?? { searches: 0, degraded: 0, slow: 0 };
    if (/tool_call name=search\b/.test(line)) {
      entry.searches++;
      if (/fully_degraded|index[:=] building/i.test(line)) entry.degraded++;
    }
    const ms = Number(line.match(SLOW_TOOL_LINE)?.[1] ?? 0);
    if (ms > TOOL_SLOW_CALL_MS) entry.slow++;
    byRoot.set(root, entry);
  }
  const out: Finding[] = [];
  for (const [root, value] of byRoot) {
    if (value.searches >= 5 && value.degraded / value.searches > 0.2) out.push(finding("search.degraded", "WARNING", `search:${root}`, `${value.degraded}/${value.searches} search calls were degraded on ${root}`, "at most 20% of search calls in the window are degraded"));
    // The daemon writes "slow tool_call" when a call COMPLETES, so every line
    // this rule can count is already history: re-raising the finding on each
    // tick of a slow episode re-reports finished calls. Report the onset once
    // per episode instead; nextPrevious records the hot roots, and the rule
    // stays quiet until a tick below the threshold ends the episode.
    if (value.slow > TOOL_SLOW_BURST && !state?.previous?.tool_slow_roots?.includes(root)) {
      out.push(finding("tool.slow", "WARNING", `tool:${root}`, `${value.slow} tool calls exceeded 10 seconds on ${root}`, "a tick with at most 10 slow calls ends the episode"));
    }
  }
  return out;
}

export function detectDeadSessions(sample: SentinelSample): Finding[] {
  if (sample.memory_error) return [instrument("memory-census", sample.memory_error)];
  const censusRoots = Object.values(sample.memory_census?.roots ?? {}) as Array<Record<string, any>>;
  return censusRoots.filter((root) => Number(root.bound_routes ?? 0) > 0 && Number(root.idle_ms ?? 0) > Number(root.root_ttl_ms ?? Infinity)).map((root) => {
    const path = String(root.project_root ?? root.root ?? "unknown");
    return finding("routes.dead_sessions", "WARNING", `route:${path}`, `${path} has ${root.bound_routes} bound route(s) after ${root.idle_ms}ms idle (TTL ${root.root_ttl_ms}ms)`, "the routes close or root activity becomes newer than its TTL");
  });
}

export function detectDsym(sample: SentinelSample): Finding[] {
  const dsym = sample.dsym;
  if (!dsym || dsym.error) return [instrument("dsym", dsym?.error ?? "running image UUID could not be inspected")];
  if (!dsym.requested_uuid) return [instrument("dsym", "running image LC_UUID is unavailable")];
  if (!dsym.path) {
    return [finding("dsym.missing", "WARNING", `dsym:${dsym.requested_uuid}`, `running image UUID ${dsym.requested_uuid} has no dSYM at its store key`, `a dSYM whose own LC_UUID is ${dsym.requested_uuid} is stored under that UUID`)];
  }
  // Something is stored under the key but we could not read a UUID out of it.
  // That is a failure of the instrument, not a determination about the
  // artifact: reporting it as staleness asserts a mismatch we never observed,
  // and the message would have to name the missing side, which historically
  // rendered as "is for an unreadable UUID, running image is <X>; re-stage" —
  // a mismatch naming one UUID and one blank. Absence above is determinate and
  // stays a WARNING; unreadability is not.
  if (!dsym.found_uuid) {
    return [instrument("dsym", `no readable DWARF under ${dsym.path}`)];
  }
  if (dsym.found_uuid !== dsym.requested_uuid) {
    return [finding("dsym.stale", "WARNING", `dsym:${dsym.requested_uuid}`, `dSYM at ${dsym.path} is for ${dsym.found_uuid}, running image is ${dsym.requested_uuid}; re-stage`, `the artifact at the running image key has LC_UUID ${dsym.requested_uuid}`)];
  }
  return [];
}

/** The branch whose scheduled runs gate nothing and are therefore watched by nobody. */
const SCHEDULED_BRANCH = "main";
// A single red scheduled run is a bad night on the runner fleet: an evicted
// build cache, a transient clone, one repository that overran its budget. A
// workflow that is genuinely broken stays broken, so the alert waits for a
// streak rather than paging on the first red. Three consecutive runs is the
// shortest streak no single bad night can produce. On a nightly cadence that
// costs two nights of delay, which is the price of not training the reader to
// ignore the channel.
const SCHEDULED_FAILURE_STREAK = 3;
// `cancelled` and `skipped` are not verdicts about the workflow's subject, and
// a run still in flight has no conclusion at all; all three are dropped before
// counting rather than being read as either outcome.
const SCHEDULED_FAILED = new Set(["failure", "timed_out", "startup_failure"]);
const SCHEDULED_SUCCEEDED = new Set(["success", "neutral"]);

/**
 * A scheduled workflow whose failures nothing else reports.
 *
 * Push and pull-request checks are read the moment someone waits on them. A
 * workflow that only runs on a schedule gates no merge, so its failures are
 * seen only if something goes looking -- and `scripts/watch-ci.sh` filters to
 * the push run on purpose, which means it cannot see them either.
 */
export function detectScheduledCi(sample: SentinelSample): Finding[] {
  if (sample.ci_error) return [instrument("scheduled-ci", sample.ci_error)];
  // collectSample always sets one of ci_runs/ci_error; undefined means this
  // sample came from a source that does not carry CI state (a --specimen file).
    if (!sample.ci_runs) return [];
    // A listing can be fetched successfully and still be stale: on 2026-09-22 the
    // live state carried a three-minute-old fetch whose newest run was a week
    // old, so this rule reported a streak that a fresh run did not reproduce.
    // Stale rows are indistinguishable from current ones once they are in hand,
    // so the age of the newest row is the only thing that can catch it — and a
    // rule that cannot tell must say so rather than judge.
    const newest = sample.ci_runs
      .map((run) => Date.parse(run.created_at ?? "") || 0)
      .reduce((left, right) => Math.max(left, right), 0);
    if (newest > 0 && sample.now_ms - newest > SCHEDULED_LISTING_MAX_AGE) {
      const days = ((sample.now_ms - newest) / 86_400_000).toFixed(1);
      return [instrument(
        "scheduled-ci",
        `the scheduled-run listing is ${days} days old; its newest run predates the poll, so no verdict is available`,
      )];
    }
  const byWorkflow = new Map<string, ScheduledRun[]>();
  const newestByWorkflow = new Map<string, number>();
  for (const run of sample.ci_runs) {
    const workflow = run.workflow?.trim();
    // A run on another branch or another trigger is a different subject.
    if (!workflow || run.event !== "schedule" || run.branch !== SCHEDULED_BRANCH) continue;
    const createdMs = Date.parse(run.created_at ?? "") || 0;
    if (createdMs > (newestByWorkflow.get(workflow) ?? 0)) newestByWorkflow.set(workflow, createdMs);
    const conclusion = String(run.conclusion ?? "");
    // A run still in flight has no conclusion yet, and `cancelled`/`skipped`
    // are not verdicts about the workflow's subject: none of the three extends
    // a streak, and none of them breaks one either.
    if (!SCHEDULED_FAILED.has(conclusion) && !SCHEDULED_SUCCEEDED.has(conclusion)) continue;
    byWorkflow.set(workflow, [...(byWorkflow.get(workflow) ?? []), run]);
  }
  const out: Finding[] = [];
  for (const [workflow, runs] of byWorkflow) {
    const newestFirst = [...runs].sort((left, right) => (Date.parse(right.created_at ?? "") || 0) - (Date.parse(left.created_at ?? "") || 0));
    let streak = 0;
    while (streak < newestFirst.length && SCHEDULED_FAILED.has(String(newestFirst[streak].conclusion ?? ""))) streak++;
    if (streak < SCHEDULED_FAILURE_STREAK) continue;
    // The verdict is per workflow, so its freshness has to be per workflow
    // too: the combined listing has served a fresh newest row from one
    // workflow while another workflow's newest runs were missing (the
    // 2026-09-23 false alarm judged the cost gate on rows whose two newest
    // successes the listing had never delivered). A workflow whose own newest
    // row predates the bound may be judged on a stale window, so the rule
    // says it cannot tell rather than reporting a streak it cannot stand
    // behind.
    const workflowNewest = newestByWorkflow.get(workflow) ?? 0;
    if (workflowNewest > 0 && sample.now_ms - workflowNewest > SCHEDULED_LISTING_MAX_AGE) {
      const days = ((sample.now_ms - workflowNewest) / 86_400_000).toFixed(1);
      out.push(instrument(
        `scheduled-ci:${workflow}`,
        `the newest scheduled run listed for "${workflow}" is ${days} days old; its listing predates the poll, so no verdict is available for this workflow`,
      ));
      continue;
    }
    // Rows kept from the combined listing after the workflow's own targeted
    // fetch failed may be missing that workflow's newest runs (the combined
    // listing has served fresh aggregate rows over stale per-workflow rows),
    // so a failure streak counted from them could be an incorrect verdict.
    if (sample.ci_fallback?.includes(workflow)) {
      out.push(instrument(
        `scheduled-ci:${workflow}`,
        `the targeted run listing for "${workflow}" could not be fetched, and the combined listing has served stale per-workflow rows, so no verdict is available for this workflow`,
      ));
      continue;
    }
    // Every run we were given failed, so the real streak reaches back past the
    // listing; say so rather than reporting the page size as the length.
    const bounded = streak === newestFirst.length;
    const oldest = newestFirst[streak - 1];
    const since = oldest.created_at ? `since ${oldest.created_at.slice(0, 10)}` : "since an unrecorded date";
    out.push(finding(
      "ci.scheduled_failing",
      "WARNING",
      `ci:${workflow}`,
      `scheduled workflow "${workflow}" has failed ${bounded ? "at least " : ""}${streak} consecutive runs on ${SCHEDULED_BRANCH}, unbroken ${since}`,
      `a scheduled run of "${workflow}" on ${SCHEDULED_BRANCH} succeeds`,
    ));
  }
  return out.sort((left, right) => left.fingerprint.localeCompare(right.fingerprint));
}

export function detectAll(sample: SentinelSample, state: SentinelState): Finding[] {
  return [
    ...detectDaemon(sample, state), ...detectLogHealth(sample), ...detectLimiter(sample, state), ...detectRetention(sample),
    ...detectIndexes(sample), ...detectTier2Overlong(sample), ...detectExecutor(sample, state), ...detectWakes(sample), ...detectWatcher(sample, state),
    ...detectStorage(sample, state), ...detectProcess(sample, state), ...detectSearchAndTools(sample, state), ...detectDeadSessions(sample), ...detectDsym(sample),
    ...detectScheduledCi(sample),
  ].filter((value, index, all) => all.findIndex((other) => other.fingerprint === value.fingerprint) === index);
}

export function reconcile(findings: Finding[], previous: FindingLedger, now: number): { raised: Finding[]; cleared: Array<{ fingerprint: string; prior: FindingLedger[string] }>; next: FindingLedger } {
  const next: FindingLedger = {};
  const raised: Finding[] = [];
  for (const current of findings) {
    const prior = previous[current.fingerprint];
    const cooldown = current.severity === "CRITICAL" ? CRITICAL_COOLDOWN : WARNING_COOLDOWN;
    // A finding that cleared and came back inside its cooldown is the same
    // subject flapping around a threshold, so the cleared entry keeps its
    // last_alerted_at and the return does not wake anyone again.
    if (!prior || now - prior.last_alerted_at >= cooldown) raised.push(current);
    next[current.fingerprint] = { last_alerted_at: !prior || now - prior.last_alerted_at >= cooldown ? now : prior.last_alerted_at, last_seen_at: now, severity: current.severity, rule: current.rule, text: current.text };
  }
  const cleared = Object.entries(previous)
    .filter(([fingerprint, prior]) => !next[fingerprint] && prior.cleared_at === undefined)
    .map(([fingerprint, prior]) => ({ fingerprint, prior }));
  for (const [fingerprint, prior] of Object.entries(previous)) {
    if (next[fingerprint]) continue;
    const cooldown = prior.severity === "CRITICAL" ? CRITICAL_COOLDOWN : WARNING_COOLDOWN;
    if (now - prior.last_alerted_at >= cooldown) continue;
    next[fingerprint] = { ...prior, cleared_at: prior.cleared_at ?? now };
  }
  return { raised, cleared, next };
}

function readJson(path: string): any { return JSON.parse(readFileSync(path, "utf8")); }
function commandJson(command: string, args: string[]): any {
  const result = spawnSync(command, args, { encoding: "utf8", timeout: 20_000 });
  if (result.status !== 0) throw new Error((result.stderr || result.stdout || `exit ${result.status}`).trim());
  return JSON.parse(result.stdout);
}
// Any `aft` process writes its own `aft-<pid>.log` into the shared logs directory
// (CLI probes, test binaries, standalone bridges), so the newest log is not
// the daemon's. The daemon is the process whose command line carries `--subc`;
// candidates are checked in mtime order and the first live daemon wins.
export function isSubcDaemon(pid: number, ps: (pid: number) => string = psArgs): boolean {
  const args = ps(pid);
  return /(^|\/)(ck-aft|aft)\b/.test(args) && /\s--subc\b/.test(args);
}
function psArgs(pid: number): string {
  const result = spawnSync("ps", ["-o", "args=", "-p", String(pid)], { encoding: "utf8" });
  return result.status === 0 ? String(result.stdout).trim() : "";
}
export function daemonPidLog(
  logDir = join(AFT, "logs"),
  ps: (pid: number) => string = psArgs,
): { path: string; pid: number } {
  const names = readdirSync(logDir).filter((name) => /^aft-\d+\.log$/.test(name));
  const paths = names.map((name) => join(logDir, name)).sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs);
  for (const path of paths) {
    const pid = Number(basename(path).match(/\d+/)?.[0]);
    if (isSubcDaemon(pid, ps)) return { path, pid };
  }
  throw new Error(`no aft pid log belongs to a running --subc daemon (${paths.length} candidate log(s))`);
}
function readNew(path: string, cursor?: { path?: string; offset?: number }): { lines: string[]; offset: number } {
  const size = statSync(path).size;
  const offset = cursor?.path === path && Number(cursor.offset ?? 0) <= size ? Number(cursor.offset ?? 0) : 0;
  const bytes = readFileSync(path).subarray(offset);
  return { lines: bytes.toString("utf8").split("\n").filter(Boolean), offset: size };
}
function sizeOf(path: string): number {
  if (!existsSync(path)) return 0;
  const stat = statSync(path);
  if (stat.isFile()) return stat.size;
  return readdirSync(path).reduce((sum, name) => sum + sizeOf(join(path, name)), 0);
}
function artifactCensus(storage: string): { sizes: Record<string, number>; roots: Record<string, string> } {
  const sizes: Record<string, number> = {};
  for (const plane of ["callgraph", "inspect", "semantic", "views"]) {
    const planePath = join(storage, plane);
    if (!existsSync(planePath)) continue;
    for (const key of readdirSync(planePath)) {
      const keyPath = join(planePath, key);
      if (!statSync(keyPath).isDirectory()) continue;
      sizes[`${plane}/${key}`] = sizeOf(keyPath);
    }
  }
  sizes["aft.db"] = sizeOf(join(storage, "aft.db")) + sizeOf(join(storage, "aft.db-wal"));

  const roots: Record<string, string> = { "aft.db": "unmapped" };
  const newest = new Map<string, { root: string; recordedAt: number }>();
  const memo = readJson(join(storage, "cache-keys.json")) as Record<string, { key?: string; recorded_at_ms?: number }>;
  for (const [root, record] of Object.entries(memo)) {
    if (!record.key) continue;
    const recordedAt = Number(record.recorded_at_ms ?? 0);
    if (!newest.has(record.key) || recordedAt > newest.get(record.key)!.recordedAt) {
      newest.set(record.key, { root, recordedAt });
    }
  }
  for (const subject of Object.keys(sizes)) {
    const key = subject.split("/")[1];
    if (key) roots[subject] = newest.get(key)?.root ?? "unmapped";
  }
  return { sizes, roots };
}

// The sentinel's launchd PATH is deliberately short and does not carry
// Homebrew, so `gh` is looked for in the usual install directories before
// falling back to whatever PATH resolves.
const GH_CANDIDATES = ["/opt/homebrew/bin/gh", "/usr/local/bin/gh", join(HOME, ".local", "bin", "gh")];
// The tick runs every two minutes; a nightly workflow changes verdict once a
// day. Polling the API on every tick would spend hundreds of requests a day to
// learn nothing, so the listing is refetched on this interval and reused in
// between.
const CI_POLL_INTERVAL = 15 * 60_000;
const CI_RUN_WINDOW = 30;
// A nightly workflow produces a run a day, so a listing whose newest row is
// older than two days is not a slow schedule, it is a listing that stopped
// tracking reality. Judging a streak from it reports last week's verdict as
// today's.
const SCHEDULED_LISTING_MAX_AGE = 2 * 24 * 60 * 60_000;

function newestRunMs(runs: ScheduledRun[]): number {
  return Math.max(0, ...runs.map((run) => Date.parse(run.created_at ?? "") || 0));
}

/**
 * Fetch the scheduled-run listing, refetching when the first answer looks stale.
 *
 * GitHub's filtered run listing (`--event schedule --branch main`) sometimes
 * serves an out-of-date result: on 2026-09-22 one call returned a listing whose
 * newest row was 2026-09-15, with every run from the following week missing,
 * while the same command minutes later (and 20 consecutive calls after that)
 * returned the 2026-09-22 run. Because a listing is reused for a whole poll
 * interval, one stale answer produced a quarter hour of "instrument unavailable"
 * warnings. When the first answer is older than the staleness bound, ask again
 * and keep the freshest listing seen. If every attempt is stale, the stale
 * listing is returned unchanged and detectScheduledCi still refuses a verdict,
 * so a listing that is genuinely behind is never judged.
 */
export function freshestScheduledListing(
  fetch: () => ScheduledRun[],
  nowMs: number,
  attempts = 3,
): ScheduledRun[] {
  let best = fetch();
  for (let attempt = 1; attempt < attempts; attempt += 1) {
    if (nowMs - newestRunMs(best) <= SCHEDULED_LISTING_MAX_AGE) break;
    const next = fetch();
    if (newestRunMs(next) > newestRunMs(best)) best = next;
  }
  return best;
}

function ghBinary(): string {
  return GH_CANDIDATES.find((path) => existsSync(path)) ?? "gh";
}

function ghRunList(args: string[]): ScheduledRun[] {
  const rows = commandJson(ghBinary(), [
    "run", "list",
    ...args,
    "--limit", String(CI_RUN_WINDOW),
    "--json", "workflowName,headBranch,event,status,conclusion,createdAt,databaseId",
  ]);
  if (!Array.isArray(rows)) throw new Error("gh run list did not return an array");
  return rows.map((row: Record<string, unknown>) => ({
    workflow: typeof row.workflowName === "string" ? row.workflowName : undefined,
    branch: typeof row.headBranch === "string" ? row.headBranch : undefined,
    event: typeof row.event === "string" ? row.event : undefined,
    status: typeof row.status === "string" ? row.status : undefined,
    conclusion: typeof row.conclusion === "string" ? row.conclusion : null,
    created_at: typeof row.createdAt === "string" ? row.createdAt : undefined,
    run_id: typeof row.databaseId === "number" ? row.databaseId : undefined,
  }));
}

function collectScheduledRuns(): ScheduledRun[] {
  return ghRunList(["--branch", SCHEDULED_BRANCH, "--event", "schedule"]);
}

/** One workflow's own scheduled runs on the watched branch. */
function collectWorkflowRuns(workflow: string): ScheduledRun[] {
  return ghRunList(["--workflow", workflow, "--branch", SCHEDULED_BRANCH, "--event", "schedule"]);
}

/**
 * Replace each workflow's rows in the combined listing with that workflow's
 * own targeted listing.
 *
 * The combined listing is judged per workflow but fetched once for all of
 * them, and GitHub's filtered listing has served rows that are fresh in
 * aggregate while an individual workflow's newest runs are missing: on
 * 2026-09-23 the combined listing's newest row came from another workflow
 * while the cost gate's two newest successes were absent, and the streak rule
 * reported 17 consecutive failures that a targeted `gh run list --workflow`
 * query showed had ended the day before. Freshness therefore has to be
 * established per workflow, so each workflow discovered in the combined
 * listing gets its own query, with the same refetch-when-stale rule as the
 * combined listing. A workflow whose targeted fetch fails keeps its combined
 * rows and is named in `fallback`, so detectScheduledCi can refuse a verdict
 * for it rather than judge it on the listing shape that produced the false
 * alarm.
 */
export function resolveWorkflowListings(
  combined: ScheduledRun[],
  fetchWorkflow: (workflow: string) => ScheduledRun[],
  nowMs: number,
): { runs: ScheduledRun[]; fallback: string[] } {
  const workflows = [...new Set(combined.map((run) => run.workflow?.trim()).filter((name): name is string => Boolean(name)))];
  const runs: ScheduledRun[] = [];
  const fallback: string[] = [];
  for (const workflow of workflows) {
    try {
      runs.push(...freshestScheduledListing(() => fetchWorkflow(workflow), nowMs));
    } catch {
      fallback.push(workflow);
      runs.push(...combined.filter((run) => run.workflow?.trim() === workflow));
    }
  }
  return { runs, fallback };
}

/**
 * Bytes the volume holding `path` can supply for important work, counting
 * purgeable space that macOS reclaims on demand.
 *
 * Source choice: `diskutil info -plist /` reports APFSContainerFree, but that
 * figure excludes purgeable space just like `df` does (both read about
 * 206 GiB on a volume where this key reads 270 GiB), so it would not fix the
 * false alarm. NSURLVolumeAvailableCapacityForImportantUsageKey is the
 * Foundation figure that includes purgeable space. It is read through
 * JavaScript for Automation (`osascript -l JavaScript`) rather than
 * `swift -e`: osascript ships with every macOS install, needs no GUI session
 * (only Foundation is bridged, no UI), and answers in about 0.2 s, while
 * `swift -e` needs the developer toolchain and compiles on every call
 * (about 1.4 s idle, far longer on a loaded machine).
 */
function availableForImportantUsage(path: string): number {
  const script = [
    'ObjC.import("Foundation");',
    "const value = Ref();",
    `const ok = $.NSURL.fileURLWithPath(${JSON.stringify(path)}).getResourceValueForKeyError(value, $.NSURLVolumeAvailableCapacityForImportantUsageKey, null);`,
    'ok ? String(value[0].js) : "";',
  ].join(" ");
  const result = spawnSync("/usr/bin/osascript", ["-l", "JavaScript", "-e", script], { encoding: "utf8", timeout: 10_000 });
  if (result.status !== 0) throw new Error((result.stderr || `osascript exit ${result.status}`).trim());
  const bytes = Number(result.stdout.trim());
  if (!result.stdout.trim() || !Number.isFinite(bytes) || bytes <= 0) {
    throw new Error(`osascript returned no capacity: ${JSON.stringify(result.stdout.trim())}`);
  }
  return bytes;
}
function diskFree(path: string): number {
  const result = spawnSync("df", ["-Pk", path], { encoding: "utf8" });
  if (result.status !== 0) throw new Error(result.stderr.trim());
  const fields = result.stdout.trim().split("\n").at(-1)?.trim().split(/\s+/) ?? [];
  return Number(fields[3]) * 1024;
}
function uuidOf(path: string): string | undefined {
  const result = spawnSync("dwarfdump", ["--uuid", path], { encoding: "utf8", timeout: 10_000 });
  return result.status === 0 ? result.stdout.match(/UUID: ([0-9A-F-]+)/i)?.[1]?.replaceAll("-", "").toUpperCase() : undefined;
}
export function collectProcessMetrics(pid: number, sample: SentinelSample): NonNullable<SentinelSample["process"]> {
  const ps = spawnSync("/bin/ps", ["-p", String(pid), "-o", "%cpu=,rss=,comm="], { encoding: "utf8", timeout: 5_000 });
  if (ps.status !== 0) throw new Error((ps.stderr || `ps exit ${ps.status}`).trim());
  const match = ps.stdout.trim().match(/^([\d.]+)\s+(\d+)\s+(.+)$/);
  if (!match) throw new Error("ps output was not parseable");
  const healthIo = healthBytesWritten(sample);
  const bytesWritten = healthIo.bytes;
  if (healthIo.available && bytesWritten === undefined) {
    throw new Error("health metrics.process_io is available but has no bytes-written counter");
  }
  return {
    pid,
    cpu_percent: Number(match[1]),
    phys_footprint_bytes: Number(metrics(sample).memory?.phys_footprint_bytes ?? Number(match[2]) * 1024),
    bytes_written: bytesWritten,
    image: match[3],
  };
}

// dwarfdump reads a .dSYM bundle by its wrapper directory, but refuses a plain
// directory ("Is a directory"), so a bundle staged without its .dSYM suffix is
// unreadable by name alone even though its DWARF is intact. Offer the wrapper,
// its children, and the Mach-O inside any of them, so a usable dSYM is found
// whatever the staging layout — an unusable one then means genuinely unusable.
function dwarfBinariesUnder(dir: string): string[] {
  const dwarf = join(dir, "Contents", "Resources", "DWARF");
  try {
    return readdirSync(dwarf).map((name) => join(dwarf, name));
  } catch {
    return [];
  }
}
function collectDsym(pid: number, image?: string): SentinelSample["dsym"] {
  const executable = image || spawnSync("ps", ["-p", String(pid), "-o", "comm="], { encoding: "utf8" }).stdout.trim();
  const requested = uuidOf(executable);
  if (!requested) return { error: `could not read LC_UUID from ${executable || `pid ${pid}`}` };
  const cache = join(AFT, "dsym", requested);
  if (!existsSync(cache)) return { requested_uuid: requested };
  let children: string[] = [];
  try {
    children = readdirSync(cache).map((name) => join(cache, name));
  } catch {
    children = [];
  }
  const candidates = [
    cache,
    ...children,
    ...dwarfBinariesUnder(cache),
    ...children.flatMap((child) => dwarfBinariesUnder(child)),
  ];
  for (const candidate of candidates) {
    const found = uuidOf(candidate);
    if (found) return { requested_uuid: requested, found_uuid: found, path: candidate };
  }
  return { requested_uuid: requested, path: cache, unreadable: true };
}
function collectSample(state: SentinelState): { sample: SentinelSample; cursors: Partial<SentinelState> } {
  const now_ms = Date.now();
  const sample: SentinelSample = { now_ms };
  const cursors: Partial<SentinelState> = {};
  const ck = existsSync(join(HOME, ".local", "bin", "ck")) ? join(HOME, ".local", "bin", "ck") : "ck";
  const readSupervisor = (): void => {
    const status = commandJson(ck, ["--subc", CONNECTION, "module", "status", "aft", "--json"]);
    const module = status.module ?? {};
    sample.supervisor = { running: module.state === "running" && module.live === true, last_exit_code: module.last_exit_code ?? null, placement_recorded: false };
    sample.health = status.health ?? {};
  };
  try {
    readSupervisor();
  } catch (error) {
    // One failed read says nothing about the daemon by itself: under load the
    // spawn times out ("exit null") while the module is running and healthy,
    // and treating that as "supervisor says not running" paged daemon.down on
    // a live daemon (2026-09-23, load 255). Confirm with the supervisor before
    // anyone may call it down: a supervisor that answers makes this a plain
    // instrument failure, and only a supervisor that is unreachable or reports
    // not running may page.
    sample.health_error = String(error);
    try {
      readSupervisor();
    } catch (confirmation) {
      sample.supervisor = { running: false };
      sample.health_error = `${String(error)}; supervisor confirmation also failed: ${String(confirmation)}`;
    }
  }
  try {
    const current = daemonPidLog();
    sample.supervisor = { ...(sample.supervisor ?? { running: true }), pid: current.pid };
    const read = readNew(current.path, state.log);
    sample.log_lines = read.lines;
    // Growth is measured only across the same file: a rotation or a new pid
    // resets the cursor to 0, and the whole file then reads as "added".
    sample.log_bytes_added = state.log?.path === current.path ? Math.max(0, read.offset - Number(state.log?.offset ?? 0)) : read.offset;
    cursors.log = { path: current.path, offset: read.offset, size: read.offset };
  } catch (error) { sample.log_error = String(error); }
  // The plugin's own structured log, not the OpenCode host log: the host log
  // quotes bash command text in permission decisions, which is not a wake.
  const pluginPath = join(AFT, "logs", "aft-plugin.log");
  try {
    const read = readNew(pluginPath, state.plugin_log);
    sample.plugin_lines = read.lines.filter((line) => WAKE_FAILURE_LINE.test(line));
    cursors.plugin_log = { path: pluginPath, offset: read.offset, size: read.offset };
  } catch (error) { sample.plugin_error = String(error); }
  try {
    const pid = sample.supervisor?.pid;
    if (!pid) throw new Error("AFT pid unavailable");
    sample.process = collectProcessMetrics(pid, sample);
  } catch (error) { sample.process_error = String(error); }
  sample.memory_census = { roots: Object.fromEntries(roots(sample).map((root) => [root.project_root ?? "unknown", root])) };
  try {
    const image = sample.process?.image;
    if (!image) throw new Error("AFT image unavailable");
    sample.writes_census = commandJson(image, ["profile", "--writes", "--since", "10m", "--json"]);
  } catch (error) { sample.writes_error = String(error); }
  try {
    const artifacts = artifactCensus(AFT);
    let available_bytes: number | undefined;
    let available_error: string | undefined;
    try { available_bytes = availableForImportantUsage(AFT); } catch (error) { available_error = String(error); }
    sample.disk = {
      free_bytes: diskFree(AFT),
      available_bytes,
      available_error,
      sizes: Object.fromEntries(["aft.db", "logs", "inspect", "callgraph", "blobs", "views"].map((name) => [name, sizeOf(join(AFT, name))])),
      artifact_sizes: artifacts.sizes,
      artifact_roots: artifacts.roots,
    };
  } catch (error) { sample.disk_error = String(error); }
  try { sample.dsym = collectDsym(sample.supervisor?.pid ?? 0, sample.process?.image); } catch (error) { sample.dsym = { error: String(error) }; }
  // A listing older than the poll interval is refetched; otherwise the stored
  // one is reused so the finding neither clears nor re-raises between polls.
  const ciAge = sample.now_ms - Number(state.ci?.checked_at_ms ?? 0);
  if (ciAge < CI_POLL_INTERVAL && (state.ci?.runs || state.ci?.error)) {
    sample.ci_runs = state.ci.runs;
    sample.ci_fallback = state.ci.fallback;
    sample.ci_error = state.ci.error;
    cursors.ci = state.ci;
  } else {
    try {
      // The combined listing discovers which scheduled workflows exist; each
      // workflow is then judged on its own targeted listing, because the
      // combined one has served fresh aggregate rows over stale per-workflow
      // rows.
      const combined = freshestScheduledListing(collectScheduledRuns, sample.now_ms);
      const resolved = resolveWorkflowListings(combined, collectWorkflowRuns, sample.now_ms);
      sample.ci_runs = resolved.runs;
      sample.ci_fallback = resolved.fallback.length > 0 ? resolved.fallback : undefined;
    } catch (error) { sample.ci_error = String(error); }
    cursors.ci = { checked_at_ms: sample.now_ms, runs: sample.ci_runs, fallback: sample.ci_fallback, error: sample.ci_error };
  }
  return { sample, cursors };
}
function readState(): SentinelState {
  try { return readJson(STATE_FILE); } catch { return { findings: {} }; }
}
function appendEvent(event: Record<string, unknown>): void {
  mkdirSync(STATE_DIR, { recursive: true });
  appendFileSync(FINDINGS_FILE, `${JSON.stringify(event)}\n`);
}
export function buildPeerDelivery(findings: Finding[], targetSession = process.env.AFT_SENTINEL_TARGET_SESSION): {
  body: string;
  urgency: "high" | "medium";
  params: Record<string, unknown>;
} {
  const sorted = [...findings].sort((left, right) => {
    const severity = Number(right.severity === "CRITICAL") - Number(left.severity === "CRITICAL");
    return severity || left.fingerprint.localeCompare(right.fingerprint);
  });
  const critical = sorted.filter((value) => value.severity === "CRITICAL").length;
  const warning = sorted.length - critical;
  const body = [
    `[AFT ${critical} CRITICAL / ${warning} WARNING]`,
    ...sorted.map((value) => `${value.severity} ${value.rule} (${value.fingerprint}): ${value.text}`),
  ].join("\n");
  return {
    body,
    urgency: critical > 0 ? "high" : "medium",
    params: {
      fromName: "AFT-SENTINEL",
      fromSessionID: "aft-health-sentinel",
      toName: "AFT",
      ...(targetSession ? { session_id: targetSession } : { agent: "AFT" }),
      body,
      urgency: critical > 0 ? "high" : "medium",
    },
  };
}

async function sendPeer(findings: Finding[]): Promise<string> {
  const delivery = buildPeerDelivery(findings);
  const identity = { project_root: process.cwd(), harness: "alfonso", session: "aft-health-sentinel" };
  const client = await SubcClient.connect({ connectionFile: CONNECTION, identity });
  try {
    const response = await client.call(
      "prefrontal-core",
      "peer.enqueue_message",
      delivery.params,
      { timeoutMs: 10_000, identity },
    );
    const result = (response as { result?: { id?: string; pmid?: string } }).result;
    const messageId = result?.pmid ?? result?.id;
    if (!messageId) throw new Error("peer.enqueue_message returned no message id");
    return messageId;
  } finally {
    client.close();
  }
}
function notify(title: string, body: string): void {
  spawnSync("osascript", ["-e", `display notification ${JSON.stringify(body)} with title ${JSON.stringify(title)}`], { timeout: 5_000 });
}
export function nextPrevious(sample: SentinelSample, state: SentinelState): SentinelState["previous"] {
  const watcher = Object.fromEntries(roots(sample).map((root) => [root.project_root ?? "unknown", [Number(root.watcher?.rescans_kernel_dropped_total ?? 0), Number(root.watcher?.rescans_user_dropped_total ?? 0)] as [number, number]]));
  const executor = executorHealth(sample);
  const previous = state.previous;
  return { pid: sample.supervisor?.pid, free_bytes: sample.disk?.free_bytes, available_bytes: sample.disk?.available_bytes, sampled_at_ms: sample.now_ms, unexplained_write_rate_runs: writeRateRunCount(sample, previous), bytes_written: sample.process?.bytes_written, sizes: sample.disk?.sizes, artifact_sizes: sample.disk?.artifact_sizes, watcher, tool_slow_roots: slowToolRoots(sample.log_lines ?? []), ...(executor.inflight > 0 && executor.workersIdle ? { phantom_inflight: true } : {}) } as SentinelState["previous"];
}

export async function main(argv = process.argv.slice(2)): Promise<number> {
  capLaunchdLogs();
  const dryRun = argv.includes("--dry-run");
  const specimenIndex = argv.indexOf("--specimen");
  const state = readState();
  let sample: SentinelSample;
  let cursors: Partial<SentinelState> = {};
  if (specimenIndex >= 0) {
    const dir = argv[specimenIndex + 1];
    sample = readJson(join(dir, "aft-health.json"));
    sample.now_ms = sample.now_ms ?? Date.now();
    sample.health = sample.health ?? sample;
    sample.log_lines = readFileSync(join(dir, "log-excerpt.txt"), "utf8").split("\n").filter(Boolean);
    sample.supervisor ??= { running: true, pid: 95277 };
  } else ({ sample, cursors } = collectSample(state));
  // The limiter rule judges a 15-minute window, but a sample only carries the
  // lines appended since the previous tick; updateLimiterWindow folds this
  // tick's lines into the persisted buffer before any detector runs, and the
  // updated buffer is both judged now and written back to the state file.
  const limiter = updateLimiterWindow(state.limiter, sample.log_lines ?? [], sample.now_ms, sample.supervisor?.pid);
  const detectionState: SentinelState = { ...state, limiter };
  const gated = gateInstrumentFindings(detectAll(sample, detectionState), state.instrument_failures);
  const findings = gated.findings;
  // The full sample is diagnostic output for a hand-run; a launchd tick
  // prints only what changed so the stdout log stays readable.
  if (dryRun) {
    console.log(JSON.stringify({ sample, findings }, null, 2));
    return 0;
  }
  const reconciled = reconcile(findings, state.findings ?? {}, sample.now_ms);
  const raised = reconciled.raised;
  for (const value of raised) {
    appendEvent({ ts: new Date(sample.now_ms).toISOString(), rule: value.rule, state: "raised", fingerprint: value.fingerprint, text: value.text, severity: value.severity });
    if (value.rule === "daemon.down" || value.fingerprint === "instrument:health-check") notify("AFT health sentinel", value.text);
  }
  if (raised.length > 0) {
    try {
      const messageId = await sendPeer(raised);
      console.error(`peer delivered id=${messageId} findings=${raised.length}`);
    } catch (error) {
      appendEvent({ ts: new Date().toISOString(), rule: "instrument", state: "raised", fingerprint: "instrument:peer-delivery", text: String(error), severity: "WARNING" });
    }
  }
  for (const value of reconciled.cleared) appendEvent({ ts: new Date(sample.now_ms).toISOString(), rule: value.prior.rule, state: "cleared", fingerprint: value.fingerprint, text: value.prior.text, severity: value.prior.severity });
  const next: SentinelState = { ...state, ...cursors, findings: reconciled.next, instrument_failures: gated.streaks, limiter, previous: nextPrevious(sample, state) };
  mkdirSync(dirname(STATE_FILE), { recursive: true });
  writeFileSync(STATE_FILE, JSON.stringify(next, null, 2));
  return 0;
}

if (import.meta.main) main().then((code) => process.exit(code)).catch((error) => { console.error(error); process.exit(1); });
