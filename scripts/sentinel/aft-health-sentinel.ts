#!/usr/bin/env bun
/** Short-lived AFT health sentinel. Collection is impure; every detector below is pure. */
import { SubcClient } from "@cortexkit/subc-client";
import { appendFileSync, existsSync, mkdirSync, readFileSync, readdirSync, statSync, truncateSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, join } from "node:path";
import { spawnSync } from "node:child_process";

export type Severity = "CRITICAL" | "WARNING";
export type Finding = { severity: Severity; rule: string; fingerprint: string; text: string; clears_when: string };
export type Plane = { status?: string; since_ms?: number | null; last_progress_at_ms?: number | null };
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
export type SentinelSample = {
  now_ms: number;
  supervisor?: { running: boolean; pid?: number; last_exit_code?: number | null; placement_recorded?: boolean };
  health?: Record<string, any>;
  health_error?: string;
  memory_census?: Record<string, any>;
  memory_error?: string;
  log_lines?: string[];
  log_error?: string;
  plugin_lines?: string[];
  plugin_error?: string;
  process?: { pid?: number; phys_footprint_bytes?: number; cpu_percent?: number; bytes_written?: number; image?: string };
  process_error?: string;
  disk?: { free_bytes?: number; sizes?: Record<string, number> };
  disk_error?: string;
  dsym?: { requested_uuid?: string; found_uuid?: string; path?: string; error?: string };
};
export type FindingLedger = Record<string, { last_alerted_at: number; last_seen_at: number; severity: Severity; rule: string; text: string }>;
export type SentinelState = {
  findings: FindingLedger;
  log?: { path?: string; offset?: number; size?: number };
  plugin_log?: { path?: string; offset?: number; size?: number };
  previous?: { pid?: number; unreachable_runs?: number; sampled_at_ms?: number; bytes_written?: number; sizes?: Record<string, number>; watcher?: Record<string, [number, number]> };
};

const HOME = homedir();
const SHARE = join(HOME, ".local", "share", "cortexkit");
const AFT = join(SHARE, "aft");
const STATE_DIR = join(HOME, ".local", "state", "cortexkit", "aft", "sentinel");
export const STATE_FILE = join(STATE_DIR, "state.json");
export const FINDINGS_FILE = join(STATE_DIR, "findings.jsonl");
const CONNECTION = join(SHARE, "run", "subc-connection.json");
const CRITICAL_COOLDOWN = 30 * 60_000;
const WARNING_COOLDOWN = 2 * 60 * 60_000;
const TEN_MINUTES = 10 * 60_000;
const FIFTEEN_MINUTES = 15 * 60_000;
const GB = 1024 ** 3;

function finding(rule: string, severity: Severity, fingerprint: string, text: string, clears_when: string): Finding {
  return { rule, severity, fingerprint, text, clears_when };
}
function instrument(name: string, detail: string): Finding {
  return finding("instrument", "WARNING", `instrument:${name}`, `instrument ${name} unavailable: ${detail}`, "the input is readable again");
}
function metrics(sample: SentinelSample): Record<string, any> { return sample.health?.metrics ?? sample.health?.health?.metrics ?? {}; }
function roots(sample: SentinelSample): RootHealth[] { return Array.isArray(metrics(sample).roots) ? metrics(sample).roots : []; }
function linesSince(lines: string[], now: number, windowMs: number): string[] {
  return lines.filter((line) => {
    const match = line.match(/^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z)/);
    if (!match) return true;
    const ts = Date.parse(match[1]);
    return !Number.isFinite(ts) || now - ts <= windowMs;
  });
}
function rootFrom(line: string): string {
  return line.match(/\broot=(.+?)(?:\s+(?:key|category|session|kind|outcome|files|ms)=|$)/)?.[1]
    ?? line.match(/route\.bind within[^:]*:\s*(\/[^ ]+)/)?.[1]
    ?? "unknown";
}

export function detectDaemon(sample: SentinelSample, state: SentinelState): Finding[] {
  const out: Finding[] = [];
  const previousMisses = state.previous?.unreachable_runs ?? 0;
  if (!sample.supervisor) out.push(instrument("supervisor", "module status was not read"));
  if (sample.health_error) out.push(instrument("health-check", sample.health_error));
  if (sample.supervisor?.running === false || (sample.health_error && previousMisses >= 1)) {
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
  if (sample.supervisor?.running && sample.health && lines.length === 0) {
    out.push(instrument("log-silent", "daemon is serving but its pid log produced zero new lines"));
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

export function detectLimiter(sample: SentinelSample): Finding[] {
  if (sample.log_error) return [instrument("limiter-log", sample.log_error)];
  const window = linesSince(sample.log_lines ?? [], sample.now_ms, FIFTEEN_MINUTES);
  const deferred = window.filter((line) => /deferred by cold build limit/i.test(line));
  const acquired = window.filter((line) => /cold-build slot acquired/i.test(line));
  if (deferred.length < 5 || acquired.length > 0) return [];
  const limiter = metrics(sample).cold_build_limiter;
  const holders: LimiterEntry[] = limiter?.holders ?? [];
  const holderText = holders.length
    ? holders.map((h) => `${h.kind ?? h.domain ?? "build"}@${h.root ?? "unknown"} age=${h.age_ms ?? "?"}ms`).join(", ")
    : [...(sample.log_lines ?? [])].reverse().filter((line) => /cold-build slot acquired/i.test(line)).slice(0, 2).map((line) => line.trim()).join(" | ") || "holders unavailable";
  return [finding("limiter.saturated", "CRITICAL", "limiter:cold-build", `${deferred.length} cold-build deferrals with no acquisition in 15m; holders: ${holderText}`, "a slot is acquired or fewer than five deferrals occur in the 15-minute window")];
}

export function detectIndexes(sample: SentinelSample): Finding[] {
  const out: Finding[] = [];
  for (const root of roots(sample)) {
    const rootPath = root.project_root ?? "unknown";
    for (const plane of ["search_index", "semantic_index", "tier2"] as const) {
      const status = root[plane];
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

export function detectExecutor(sample: SentinelSample, state: SentinelState): Finding[] {
  const dispatch = metrics(sample).dispatch_liveness;
  if (!dispatch) return [instrument("executor-health", "dispatch_liveness is absent")];
  const zombie = Number(dispatch.executor_zombie_reader ?? 0);
  const inflight = Number(dispatch.maintenance_inflight ?? dispatch.running?.maintenance ?? 0);
  const workersIdle = Number(dispatch.running?.interactive ?? 0) === 0 && Number(dispatch.running?.maintenance ?? 0) === 0;
  const priorPhantom = state.previous?.sampled_at_ms && (state as any).previous?.phantom_inflight;
  if (zombie > 0 || (inflight > 0 && workersIdle && priorPhantom)) {
    return [finding("executor.phantom", "CRITICAL", "executor:maintenance", `executor health reports zombie_reader=${zombie}, maintenance_inflight=${inflight} while workers are idle`, "zombie readers are zero and maintenance in-flight agrees with active workers")];
  }
  return [];
}

export function detectWakes(sample: SentinelSample): Finding[] {
  const runtime = metrics(sample).runtime ?? {};
  const oldest = Number(runtime.bg_wake_oldest_unacked_age_ms ?? 0);
  const failures = (sample.plugin_lines ?? []).filter((line) => /this\._client|promptAsync/i.test(line));
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
  if (free < 40 * GB) out.push(finding("disk.low", "CRITICAL", "disk:aft-data", `AFT data volume has ${(free / GB).toFixed(1)} GiB free`, "free space reaches 40 GiB"));
  else if (free < 80 * GB) out.push(finding("disk.low", "WARNING", "disk:aft-data", `AFT data volume has ${(free / GB).toFixed(1)} GiB free`, "free space reaches 80 GiB"));
  for (const [name, size] of Object.entries(sample.disk?.sizes ?? {})) {
    const prior = state.previous?.sizes?.[name];
    if (typeof prior === "number" && size - prior > 5 * GB) {
      out.push(finding("storage.growth", "WARNING", `storage:${name}`, `${name} grew by ${((size - prior) / GB).toFixed(1)} GiB in one interval`, "the next interval grows by at most 5 GiB"));
    }
  }
  return out;
}

export function detectProcess(sample: SentinelSample, state: SentinelState): Finding[] {
  if (sample.process_error) return [instrument("process", sample.process_error)];
  const out: Finding[] = [];
  const proc = sample.process ?? {};
  if (Number(proc.phys_footprint_bytes ?? 0) > 6 * GB) out.push(finding("process.footprint", "WARNING", `process:${proc.pid ?? "aft"}:footprint`, `AFT physical footprint is ${(Number(proc.phys_footprint_bytes) / GB).toFixed(1)} GiB`, "physical footprint is at most 6 GiB"));
  if (Number(proc.cpu_percent ?? 0) > 150) out.push(finding("process.cpu", "WARNING", `process:${proc.pid ?? "aft"}:cpu`, `AFT CPU averaged ${proc.cpu_percent}% over the interval`, "interval CPU is at most 150%"));
  const previous = state.previous;
  if (typeof proc.bytes_written === "number" && typeof previous?.bytes_written === "number" && previous.sampled_at_ms) {
    const hours = Math.max(1 / 3600, (sample.now_ms - previous.sampled_at_ms) / 3_600_000);
    const rate = (proc.bytes_written - previous.bytes_written) / hours;
    if (rate > GB) out.push(finding("process.writes", "WARNING", `process:${proc.pid ?? "aft"}:writes`, `AFT physical write rate is ${(rate / GB).toFixed(1)} GiB/h`, "physical write rate is at most 1 GiB/h"));
  } else if (proc.bytes_written === undefined) out.push(instrument("process-writes", "proc_pid_rusage bytes_written is unavailable"));
  return out;
}

export function detectSearchAndTools(sample: SentinelSample): Finding[] {
  if (sample.log_error) return [instrument("tool-log", sample.log_error)];
  const byRoot = new Map<string, { searches: number; degraded: number; slow: number }>();
  for (const line of sample.log_lines ?? []) {
    const root = rootFrom(line);
    const entry = byRoot.get(root) ?? { searches: 0, degraded: 0, slow: 0 };
    if (/tool_call name=search\b/.test(line)) {
      entry.searches++;
      if (/fully_degraded|index[:=] building/i.test(line)) entry.degraded++;
    }
    const ms = Number(line.match(/slow tool_call .*\btotal=(\d+)ms/)?.[1] ?? 0);
    if (ms > 10_000) entry.slow++;
    byRoot.set(root, entry);
  }
  const out: Finding[] = [];
  for (const [root, value] of byRoot) {
    if (value.searches >= 5 && value.degraded / value.searches > 0.2) out.push(finding("search.degraded", "WARNING", `search:${root}`, `${value.degraded}/${value.searches} search calls were degraded on ${root}`, "at most 20% of search calls in the window are degraded"));
    if (value.slow > 10) out.push(finding("tool.slow", "WARNING", `tool:${root}`, `${value.slow} tool calls exceeded 10 seconds on ${root}`, "at most 10 tool calls exceed 10 seconds in the window"));
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
  if (dsym.requested_uuid && dsym.found_uuid !== dsym.requested_uuid) {
    return [finding("dsym.missing", "WARNING", `dsym:${dsym.requested_uuid}`, `running image UUID ${dsym.requested_uuid} has no matching dSYM; found ${dsym.found_uuid ?? "none"}${dsym.path ? ` at ${dsym.path}` : ""}`, `a dSYM whose own LC_UUID is ${dsym.requested_uuid} is stored under that UUID`)];
  }
  return [];
}

export function detectAll(sample: SentinelSample, state: SentinelState): Finding[] {
  return [
    ...detectDaemon(sample, state), ...detectLogHealth(sample), ...detectLimiter(sample), ...detectIndexes(sample),
    ...detectTier2Overlong(sample), ...detectExecutor(sample, state), ...detectWakes(sample), ...detectWatcher(sample, state),
    ...detectStorage(sample, state), ...detectProcess(sample, state), ...detectSearchAndTools(sample), ...detectDeadSessions(sample), ...detectDsym(sample),
  ].filter((value, index, all) => all.findIndex((other) => other.fingerprint === value.fingerprint) === index);
}

export function reconcile(findings: Finding[], previous: FindingLedger, now: number): { raised: Finding[]; cleared: Array<{ fingerprint: string; prior: FindingLedger[string] }>; next: FindingLedger } {
  const next: FindingLedger = {};
  const raised: Finding[] = [];
  for (const current of findings) {
    const prior = previous[current.fingerprint];
    const cooldown = current.severity === "CRITICAL" ? CRITICAL_COOLDOWN : WARNING_COOLDOWN;
    if (!prior || now - prior.last_alerted_at >= cooldown) raised.push(current);
    next[current.fingerprint] = { last_alerted_at: !prior || now - prior.last_alerted_at >= cooldown ? now : prior.last_alerted_at, last_seen_at: now, severity: current.severity, rule: current.rule, text: current.text };
  }
  const cleared = Object.entries(previous).filter(([fingerprint]) => !next[fingerprint]).map(([fingerprint, prior]) => ({ fingerprint, prior }));
  return { raised, cleared, next };
}

function readJson(path: string): any { return JSON.parse(readFileSync(path, "utf8")); }
function commandJson(command: string, args: string[]): any {
  const result = spawnSync(command, args, { encoding: "utf8", timeout: 20_000 });
  if (result.status !== 0) throw new Error((result.stderr || result.stdout || `exit ${result.status}`).trim());
  return JSON.parse(result.stdout);
}
function newestPidLog(): { path: string; pid: number } {
  const names = readdirSync(join(AFT, "logs")).filter((name) => /^aft-\d+\.log$/.test(name));
  const path = names.map((name) => join(AFT, "logs", name)).sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];
  if (!path) throw new Error("no aft pid log");
  return { path, pid: Number(basename(path).match(/\d+/)?.[0]) };
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
function procRusage(pid: number): { phys_footprint_bytes: number; bytes_written: number } {
  if (process.platform !== "darwin") throw new Error("proc_pid_rusage sampling is implemented for macOS only");
  const source = `
import ctypes, json, sys
class RusageInfoV4(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_ubyte * 16)] + [(name, ctypes.c_uint64) for name in (
      "user_time", "system_time", "pkg_idle_wkups", "interrupt_wkups", "pageins",
      "wired_size", "resident_size", "phys_footprint", "proc_start_abstime", "proc_exit_abstime",
      "child_user_time", "child_system_time", "child_pkg_idle_wkups", "child_interrupt_wkups",
      "child_pageins", "child_elapsed_abstime", "diskio_bytesread", "diskio_byteswritten")]
info = RusageInfoV4()
result = ctypes.CDLL("/usr/lib/libproc.dylib").proc_pid_rusage(int(sys.argv[1]), 4, ctypes.byref(info))
if result != 0: raise OSError(ctypes.get_errno(), "proc_pid_rusage failed")
print(json.dumps({"phys_footprint_bytes": info.phys_footprint, "bytes_written": info.diskio_byteswritten}))
  `;
  const result = spawnSync("python3", ["-c", source, String(pid)], { encoding: "utf8", timeout: 5_000 });
  if (result.status !== 0) throw new Error((result.stderr || `python exit ${result.status}`).trim());
  return JSON.parse(result.stdout);
}

function collectDsym(pid: number, image?: string): SentinelSample["dsym"] {
  const executable = image || spawnSync("ps", ["-p", String(pid), "-o", "comm="], { encoding: "utf8" }).stdout.trim();
  const requested = uuidOf(executable);
  if (!requested) return { error: `could not read LC_UUID from ${executable || `pid ${pid}`}` };
  const cache = join(AFT, "dsym", requested);
  if (!existsSync(cache)) return { requested_uuid: requested };
  const candidates = [cache, ...readdirSync(cache).map((name) => join(cache, name))];
  for (const candidate of candidates) {
    const found = uuidOf(candidate);
    if (found) return { requested_uuid: requested, found_uuid: found, path: candidate };
  }
  return { requested_uuid: requested, path: cache };
}
function collectSample(state: SentinelState): { sample: SentinelSample; cursors: Partial<SentinelState> } {
  const now_ms = Date.now();
  const sample: SentinelSample = { now_ms };
  const cursors: Partial<SentinelState> = {};
  const ck = existsSync(join(HOME, ".local", "bin", "ck")) ? join(HOME, ".local", "bin", "ck") : "ck";
  try {
    const status = commandJson(ck, ["--subc", CONNECTION, "module", "status", "aft", "--json"]);
    const module = status.module ?? {};
    sample.supervisor = { running: module.state === "running" && module.live === true, last_exit_code: module.last_exit_code ?? null, placement_recorded: false };
    sample.health = status.health ?? {};
  } catch (error) { sample.health_error = String(error); sample.supervisor = { running: false }; }
  try {
    const current = newestPidLog();
    sample.supervisor = { ...(sample.supervisor ?? { running: true }), pid: current.pid };
    const read = readNew(current.path, state.log);
    sample.log_lines = read.lines;
    cursors.log = { path: current.path, offset: read.offset, size: read.offset };
  } catch (error) { sample.log_error = String(error); }
  const pluginPath = join(HOME, ".local", "share", "opencode", "log", "opencode.log");
  try {
    const read = readNew(pluginPath, state.plugin_log);
    sample.plugin_lines = read.lines.filter((line) => /this\._client|promptAsync/i.test(line));
    cursors.plugin_log = { path: pluginPath, offset: read.offset, size: read.offset };
  } catch (error) { sample.plugin_error = String(error); }
  try {
    const pid = sample.supervisor?.pid;
    if (!pid) throw new Error("AFT pid unavailable");
    const ps = spawnSync("ps", ["-p", String(pid), "-o", "%cpu=,rss=,comm="], { encoding: "utf8" });
    if (ps.status !== 0) throw new Error(ps.stderr.trim());
    const match = ps.stdout.trim().match(/^([\d.]+)\s+(\d+)\s+(.+)$/);
    if (!match) throw new Error("ps output was not parseable");
    const rusage = procRusage(pid);
    sample.process = { pid, cpu_percent: Number(match[1]), phys_footprint_bytes: rusage.phys_footprint_bytes || Number(metrics(sample).memory?.phys_footprint_bytes ?? Number(match[2]) * 1024), bytes_written: rusage.bytes_written, image: match[3] };
  } catch (error) { sample.process_error = String(error); }
  sample.memory_census = { roots: Object.fromEntries(roots(sample).map((root) => [root.project_root ?? "unknown", root])) };
  try {
    sample.disk = { free_bytes: diskFree(AFT), sizes: Object.fromEntries(["aft.db", "logs", "inspect", "callgraph", "blobs", "views"].map((name) => [name, sizeOf(join(AFT, name))])) };
  } catch (error) { sample.disk_error = String(error); }
  try { sample.dsym = collectDsym(sample.supervisor?.pid ?? 0, sample.process?.image); } catch (error) { sample.dsym = { error: String(error) }; }
  return { sample, cursors };
}
function readState(): SentinelState {
  try { return readJson(STATE_FILE); } catch { return { findings: {} }; }
}
function appendEvent(event: Record<string, unknown>): void {
  mkdirSync(STATE_DIR, { recursive: true });
  appendFileSync(FINDINGS_FILE, `${JSON.stringify(event)}\n`);
}
async function sendPeer(findingValue: Finding): Promise<string> {
  const prefrontal = join(HOME, "Work", "Projects", "CortexKit", "prefrontal");
  if (!existsSync(join(prefrontal, "script", "health-sentinel.ts"))) throw new Error(`registry sentinel missing under ${prefrontal}`);
  let target = process.env.AFT_SENTINEL_TARGET_SESSION;
  if (!target) {
    try {
      const raw = readFileSync(join(prefrontal, ".cortexkit", "alfonso", "health-sentinel.jsonc"), "utf8");
      target = raw.match(/"targetSessionId"\s*:\s*"([^"]+)"/)?.[1];
    } catch { /* surfaced by the explicit target check below */ }
  }
  if (!target) throw new Error("AFT_SENTINEL_TARGET_SESSION and the registry sentinel target are unset");
  const identity = { project_root: prefrontal, harness: "alfonso", session: "prefrontal-core" };
  const client = await SubcClient.connect({ connectionFile: CONNECTION, identity });
  try {
    const response = await client.call(
      "prefrontal-core",
      "peer.enqueue_message",
      {
        fromName: "AFT-SENTINEL",
        fromSessionID: "aft-health-sentinel",
        toName: "ALF",
        toSessionID: target,
        toDirectory: "",
        body: `[AFT ${findingValue.severity}] ${findingValue.text}`,
        urgency: findingValue.severity === "CRITICAL" ? "high" : "medium",
      },
      { timeoutMs: 10_000, identity },
    );
    const messageId = (response as { result?: { id?: string } }).result?.id;
    if (!messageId) throw new Error("peer.enqueue_message returned no message id");
    return messageId;
  } finally {
    client.close();
  }
}
function notify(title: string, body: string): void {
  spawnSync("osascript", ["-e", `display notification ${JSON.stringify(body)} with title ${JSON.stringify(title)}`], { timeout: 5_000 });
}
function nextPrevious(sample: SentinelSample, state: SentinelState): SentinelState["previous"] {
  const watcher = Object.fromEntries(roots(sample).map((root) => [root.project_root ?? "unknown", [Number(root.watcher?.rescans_kernel_dropped_total ?? 0), Number(root.watcher?.rescans_user_dropped_total ?? 0)] as [number, number]]));
  const dispatch = metrics(sample).dispatch_liveness;
  const previous = state.previous;
  return { pid: sample.supervisor?.pid, unreachable_runs: sample.health_error ? (previous?.unreachable_runs ?? 0) + 1 : 0, sampled_at_ms: sample.now_ms, bytes_written: sample.process?.bytes_written, sizes: sample.disk?.sizes, watcher, ...(dispatch?.maintenance_inflight > 0 && dispatch?.running?.maintenance === 0 ? { phantom_inflight: true } : {}) } as SentinelState["previous"];
}

export async function main(argv = process.argv.slice(2)): Promise<number> {
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
  const findings = detectAll(sample, state);
  console.log(JSON.stringify({ sample, findings }, null, 2));
  if (dryRun) return 0;
  const reconciled = reconcile(findings, state.findings ?? {}, sample.now_ms);
  for (const value of reconciled.raised) {
    appendEvent({ ts: new Date(sample.now_ms).toISOString(), rule: value.rule, state: "raised", fingerprint: value.fingerprint, text: value.text, severity: value.severity });
    try {
      const messageId = await sendPeer(value);
      console.error(`peer delivered id=${messageId} fingerprint=${value.fingerprint}`);
    } catch (error) { appendEvent({ ts: new Date().toISOString(), rule: "instrument", state: "raised", fingerprint: "instrument:peer-delivery", text: String(error), severity: "WARNING" }); }
    if (value.rule === "daemon.down" || value.fingerprint === "instrument:health-check") notify("AFT health sentinel", value.text);
  }
  for (const value of reconciled.cleared) appendEvent({ ts: new Date(sample.now_ms).toISOString(), rule: value.prior.rule, state: "cleared", fingerprint: value.fingerprint, text: value.prior.text, severity: value.prior.severity });
  const next: SentinelState = { ...state, ...cursors, findings: reconciled.next, previous: nextPrevious(sample, state) };
  mkdirSync(dirname(STATE_FILE), { recursive: true });
  writeFileSync(STATE_FILE, JSON.stringify(next, null, 2));
  return 0;
}

if (import.meta.main) main().then((code) => process.exit(code)).catch((error) => { console.error(error); process.exit(1); });
