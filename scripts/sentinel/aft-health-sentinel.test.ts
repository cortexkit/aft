import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import {
  detectAll,
  detectDaemon,
  detectDeadSessions,
  detectDsym,
  detectExecutor,
  detectIndexes,
  detectLimiter,
  detectLogHealth,
  detectProcess,
  detectSearchAndTools,
  detectStorage,
  detectTier2Overlong,
  detectWakes,
  detectWatcher,
  healthBytesWritten,
  reconcile,
  type SentinelSample,
  type SentinelState,
} from "./aft-health-sentinel";

const NOW = Date.parse("2026-09-17T14:30:00Z");
const cleanState = (): SentinelState => ({ findings: {}, previous: { sampled_at_ms: NOW - 120_000 } });
const sample = (patch: Partial<SentinelSample> = {}): SentinelSample => ({
  now_ms: NOW,
  supervisor: { running: true, pid: 42 },
  health: { metrics: { roots: [], runtime: {}, dispatch_liveness: { running: { interactive: 0, maintenance: 0 } } } },
  log_lines: ["2026-09-17T14:29:59Z [aft] heartbeat"],
  plugin_lines: [],
  process: { pid: 42, phys_footprint_bytes: 1, cpu_percent: 1, bytes_written: 1 },
  disk: { free_bytes: 100 * 1024 ** 3, sizes: {} },
  memory_census: { roots: {} },
  dsym: { requested_uuid: "AA", found_uuid: "AA" },
  ...patch,
});
const rules = (values: ReturnType<typeof detectAll>) => values.map((value) => value.rule);

// These cases keep detector thresholds executable rather than duplicating the
// prose contract in a second hand-maintained table.
describe("health sentinel pure detectors", () => {
  test("daemon.down requires two unreachable health runs", () => {
    const first = detectDaemon(sample({ health: undefined, health_error: "timeout" }), cleanState());
    expect(first.some((value) => value.rule === "daemon.down")).toBe(false);
    const second = detectDaemon(sample({ health: undefined, health_error: "timeout" }), { findings: {}, previous: { unreachable_runs: 1 } });
    expect(second.find((value) => value.rule === "daemon.down")?.fingerprint).toBe("daemon:aft");
  });

  test("daemon restart, panic, bind stall, and sandbox refusal are surfaced", () => {
    expect(rules(detectAll(sample({ supervisor: { running: true, pid: 43, last_exit_code: 9 }, log_lines: [
      "2026-09-17T14:29:00Z [aft] panicked at boom",
      "2026-09-17T14:29:01Z [aft] did not answer route.bind within 10s: /tmp/root",
      "2026-09-17T14:29:02Z [aft] sandbox setup for git config failed: EBADF root=/tmp/root",
    ] }), { findings: {}, previous: { pid: 42 } }))).toEqual(expect.arrayContaining(["daemon.restarted", "daemon.panic", "bind.stall", "sandbox.refusal"]));
  });

  test("serving daemon with no new pid-log lines is an instrument warning", () => {
    expect(detectLogHealth(sample({ log_lines: [] }))[0].fingerprint).toBe("instrument:log-silent");
  });

  test("limiter saturation raises without turnover", () => {
    const deferrals = Array.from({ length: 5 }, (_, index) => `2026-09-17T14:2${index}:00Z [aft] tier2 refresh deferred by cold build limit`);
    expect(detectLimiter(sample({ log_lines: deferrals }))[0].fingerprint).toBe("limiter:cold-build");
  });

  test("healthy post-restart limiter turnover does not raise", () => {
    const fixture = readFileSync(join(import.meta.dir, "fixtures", "healthy-limiter.log"), "utf8").trim().split("\n");
    expect(detectLimiter(sample({ log_lines: fixture }))).toEqual([]);
  });

  test("stuck indexes use stable root and plane fingerprint", () => {
    const root = "/repo/magic-context";
    const input = sample({ health: { metrics: { roots: [{ project_root: root, search_index: { status: "building", since_ms: NOW - 700_000, last_progress_at_ms: NOW - 700_000 } }] } } });
    expect(detectIndexes(input)[0].fingerprint).toBe(`index:${root}:search`);
    expect(detectIndexes({ ...input, now_ms: NOW + 1 })[0].fingerprint).toBe(`index:${root}:search`);
    expect(detectIndexes(sample({ health: { metrics: { roots: [{ project_root: root, search_index: { status: "ready" } }] } } }))).toEqual([]);
  });

  test("overlong tier2 pass requires an unmatched old acquisition", () => {
    const root = "/repo/worktree";
    expect(detectTier2Overlong(sample({ log_lines: [`2026-09-17T14:00:00Z [aft] inspect-triggered cold-build slot acquired after 1ms wait: request=inspect:${root}:1 kind=explicit inspect Tier-2 run`] }))[0].fingerprint).toBe(`tier2:${root}`);
    expect(detectTier2Overlong(sample({ log_lines: [
      `2026-09-17T14:00:00Z [aft] inspect-triggered cold-build slot acquired after 1ms wait: request=inspect:${root}:1 kind=explicit inspect Tier-2 run`,
      `2026-09-17T14:01:00Z [aft] perf tier2 phases category=dead_code root=${root} key=a`,
    ] }))).toEqual([]);
  });

  test("executor phantom needs persistence while zombie readers fire immediately", () => {
    const zombie = sample({ health: { metrics: { dispatch_liveness: { executor_zombie_reader: 1, running: { interactive: 0, maintenance: 0 } } } } });
    expect(detectExecutor(zombie, cleanState())[0].rule).toBe("executor.phantom");
  });

  test("wake failures and stale unacked completions raise", () => {
    expect(detectWakes(sample({ plugin_lines: ["TypeError: this._client is undefined"] }))[0].rule).toBe("wakes.backlog");
  });

  test("watcher overflow compares counters to prior sample", () => {
    const root = "/repo";
    const input = sample({ health: { metrics: { roots: [{ project_root: root, watcher: { rescans_kernel_dropped_total: 2, rescans_user_dropped_total: 1 } }] } } });
    expect(detectWatcher(input, { findings: {}, previous: { watcher: { [root]: [1, 1] } } })[0].fingerprint).toBe(`watcher:${root}`);
  });

  test("disk thresholds and per-path growth raise and clear", () => {
    const state: SentinelState = { findings: {}, previous: { sizes: { inspect: 1 } } };
    const raised = detectStorage(sample({ disk: { free_bytes: 30 * 1024 ** 3, sizes: { inspect: 6 * 1024 ** 3 + 2 } } }), state);
    expect(rules(raised)).toEqual(expect.arrayContaining(["disk.low", "storage.growth"]));
    expect(detectStorage(sample(), cleanState())).toEqual([]);
  });

  test("health process_io prefers physical writes and falls back to logical writes", () => {
    expect(healthBytesWritten(sample({ health: { metrics: { process_io: { available: true, diskio_bytes_written: 42, logical_bytes_written: 99 } } } }))).toEqual({ available: true, bytes: 42 });
    expect(healthBytesWritten(sample({ health: { metrics: { process_io: { available: true, logical_bytes_written: 99 } } } }))).toEqual({ available: true, bytes: 99 });
    expect(healthBytesWritten(sample({ health: { metrics: { process_io: { available: false } } } }))).toEqual({ available: false });
  });

  test("process footprint, cpu, and write rate are independent", () => {
    const state: SentinelState = { findings: {}, previous: { sampled_at_ms: NOW - 3_600_000, bytes_written: 0 } };
    expect(rules(detectProcess(sample({ process: { pid: 42, phys_footprint_bytes: 7 * 1024 ** 3, cpu_percent: 151, bytes_written: 2 * 1024 ** 3 } }), state))).toEqual(expect.arrayContaining(["process.footprint", "process.cpu", "process.writes"]));
  });

  test("degraded search ratio and slow-call count are root scoped", () => {
    const lines = [
      ...Array.from({ length: 5 }, () => "2026-09-17T14:29:00Z [aft] slow tool_call name=search total=100ms root=/repo fully_degraded"),
      ...Array.from({ length: 11 }, () => "2026-09-17T14:29:00Z [aft] slow tool_call name=read total=11000ms root=/repo"),
    ];
    expect(rules(detectSearchAndTools(sample({ log_lines: lines })))).toEqual(["search.degraded", "tool.slow"]);
  });

  test("dead routed sessions compare idle age with root TTL", () => {
    const input = sample({ memory_census: { roots: { a: { project_root: "/repo", bound_routes: 1, idle_ms: 61, root_ttl_ms: 60 } } } });
    expect(detectDeadSessions(input)[0].fingerprint).toBe("route:/repo");
  });

  test("mismatched dSYM under requested UUID key is refused", () => {
    const input = sample({ dsym: { requested_uuid: "2CD06659", found_uuid: "E570EF4A", path: "/dsym/2CD06659/aft.dSYM" } });
    const result = detectDsym(input)[0];
    expect(result.rule).toBe("dsym.missing");
    expect(result.text).toContain("2CD06659");
    expect(result.text).toContain("E570EF4A");
  });

  test("dedupe alerts once, clears absent fingerprints, and re-alerts after clear", () => {
    const current = detectStorage(sample({ disk: { free_bytes: 30 * 1024 ** 3, sizes: {} } }), cleanState())[0];
    const first = reconcile([current], {}, NOW);
    expect(first.raised).toHaveLength(1);
    const second = reconcile([current], first.next, NOW + 1);
    expect(second.raised).toHaveLength(0);
    const clear = reconcile([], second.next, NOW + 2);
    expect(clear.cleared.map((value) => value.fingerprint)).toEqual([current.fingerprint]);
    expect(reconcile([current], clear.next, NOW + 3).raised).toHaveLength(1);
  });
});

test("preserved tier2 wedge replay raises the three causal findings", () => {
  const external = join(homedir(), ".local", "share", "cortexkit", "aft", "wedge-specimens", "2026-09-17-tier2-spin");
  const health = JSON.parse(readFileSync(join(external, "aft-health.json"), "utf8"));
  const logLines = readFileSync(join(external, "log-excerpt.txt"), "utf8").trim().split("\n");
  const lastTs = Math.max(...logLines.map((line) => Date.parse(line.split(" ", 1)[0])).filter(Number.isFinite));
  const magic = health.metrics.roots.find((root: any) => root.project_root?.endsWith("/magic-context"));
  magic.search_index.since_ms = lastTs - 20 * 60_000;
  magic.search_index.last_progress_at_ms = lastTs - 20 * 60_000;
  const worktree = "/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/bg_9829eba94c8c352e";
  const replay: SentinelSample = sample({ now_ms: lastTs, health, log_lines: logLines });
  const limiterLines = logLines.filter((line) => /deferred by cold build limit/.test(line) || (/cold-build slot acquired/.test(line) && Date.parse(line.split(" ", 1)[0]) > lastTs - 15 * 60_000));
  replay.log_lines = [...limiterLines, `2026-09-17T10:42:27Z [aft] inspect-triggered cold-build slot acquired after 1ms wait: request=inspect:${worktree}:1 kind=explicit inspect Tier-2 run`];
  const found = [...detectLimiter({ ...replay, log_lines: limiterLines }), ...detectIndexes(replay), ...detectTier2Overlong({ ...replay, log_lines: replay.log_lines })];
  expect(rules(found)).toEqual(expect.arrayContaining(["limiter.saturated", "index.stuck", "tier2.pass_overlong"]));
});
