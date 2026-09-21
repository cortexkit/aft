import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, statSync, utimesSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import {
  buildPeerDelivery,
  capLaunchdLogs,
  collectProcessMetrics,
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
  detectScheduledCi,
  detectStorage,
  detectTier2Overlong,
  detectWakes,
  detectWatcher,
  healthBytesWritten,
  reconcile,
  STATE_DIR,
  writeGrowthAttribution,
  type SentinelSample,
  type SentinelState,
  type ScheduledRun,
  daemonPidLog,
  isSubcDaemon,
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
  dsym: { requested_uuid: "AA", found_uuid: "AA", path: "/dsym/AA/aft.dSYM" },
  ...patch,
});
const rules = (values: ReturnType<typeof detectAll>) => values.map((value) => value.rule);

// These cases keep detector thresholds executable rather than duplicating the
// prose contract in a second hand-maintained table.
describe("health sentinel pure detectors", () => {
  test("test imports use a temporary sentinel state directory", () => {
    expect(STATE_DIR.startsWith(tmpdir())).toBe(true);
    expect(STATE_DIR).toContain("aft-health-sentinel-test-");
  });

  test("process collection does not depend on python in PATH", () => {
    const oldPath = process.env.PATH;
    process.env.PATH = "/nonexistent";
    try {
      const collected = collectProcessMetrics(process.pid, sample({ health: { metrics: { process_io: { available: true, diskio_bytes_written: 123 } } } }));
      expect(collected.bytes_written).toBe(123);
      expect(collected.pid).toBe(process.pid);
    } finally {
      process.env.PATH = oldPath;
    }
  });

  test("peer delivery addresses AFT by registry name and folds severity-sorted findings", () => {
    const warning = { rule: "disk.low", severity: "WARNING" as const, fingerprint: "disk:data", text: "warning", clears_when: "space" };
    const critical = { rule: "daemon.down", severity: "CRITICAL" as const, fingerprint: "daemon:aft", text: "critical", clears_when: "up" };
    const delivery = buildPeerDelivery([warning, critical], undefined);
    expect(delivery.params.toName).toBe("AFT");
    expect(delivery.params.toName).not.toBe("ALF");
    expect(delivery.params.agent).toBe("AFT");
    expect(delivery.params).not.toHaveProperty("session_id");
    expect(delivery.urgency).toBe("high");
    expect(delivery.body.split("\n")).toEqual([
      "[AFT 1 CRITICAL / 1 WARNING]",
      "CRITICAL daemon.down (daemon:aft): critical",
      "WARNING disk.low (disk:data): warning",
    ]);
    const override = buildPeerDelivery([warning], "session-aft-override");
    expect(override.params.toName).toBe("AFT");
    expect(override.params.session_id).toBe("session-aft-override");
    expect(override.params).not.toHaveProperty("agent");
  });

  test("launchd logs are truncated after one MiB", () => {
    const dir = mkdtempSync(join(tmpdir(), "aft-sentinel-logs-"));
    const small = join(dir, "small.log");
    const large = join(dir, "large.log");
    writeFileSync(small, "kept");
    writeFileSync(large, Buffer.alloc(1024 * 1024 + 1));
    capLaunchdLogs([small, large]);
    expect(readFileSync(small, "utf8")).toBe("kept");
    expect(statSync(large).size).toBe(0);
  });

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

  test("a silent log is only an instrument warning when the log actually grew", () => {
    // An idle daemon writes nothing, and that is not a fault to report.
    expect(detectLogHealth(sample({ log_lines: [], log_bytes_added: 0 }))).toEqual([]);
    // Bytes appended that yield no lines means we cannot read what was written.
    const broken = detectLogHealth(sample({ log_lines: [], log_bytes_added: 4096 }));
    expect(broken[0].fingerprint).toBe("instrument:log-silent");
    expect(broken[0].text).toContain("4096");
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

  test("backend outage raises once and suppresses per-root stuck findings", () => {
    const roots = ["/repo/prefrontal", "/repo/magic-context"];
    const input = sample({ health: { metrics: {
      embedding_backend: { available: false, last_error: "connection refused", since_ms: NOW - 700_000 },
      roots: roots.map((project_root) => ({
        project_root,
        semantic_index: {
          status: "backend_unavailable",
          reason: "connection refused",
          since_ms: NOW - 700_000,
          next_retry_ms: NOW + 30_000,
        },
      })),
    } } });

    const findings = detectIndexes(input);
    expect(findings.filter((value) => value.rule === "embedding.backend_down")).toHaveLength(1);
    expect(findings.filter((value) => value.rule === "index.stuck")).toHaveLength(0);
    expect(findings[0].severity).toBe("WARNING");
    expect(findings[0].text).toContain(roots.join(", "));
  });

  test("building index with recent progress is not stuck", () => {
    const input = sample({ health: { metrics: { roots: [{
      project_root: "/repo/active",
      tier2: { status: "building", since_ms: NOW - 700_000, last_progress_at_ms: NOW - 30_000 },
    }] } } });
    expect(detectIndexes(input)).toEqual([]);
  });

  test("tier2 refresh overdue waits through the five-minute grace and clears on dispatch states", () => {
    const root = "/repo/busy";
    const nextRefreshAt = NOW - 5 * 60_000;
    const staleHealth = (now_ms: number) => sample({
      now_ms,
      health: { metrics: { roots: [{
        project_root: root,
        tier2: { status: "stale", stale_since_ms: NOW - 20 * 60_000, next_refresh_at_ms: nextRefreshAt, pending_paths: 3 },
      }] } },
    });

    expect(detectIndexes(staleHealth(NOW))).toEqual([]);
    const overdue = detectIndexes(staleHealth(NOW + 1));
    expect(overdue).toHaveLength(1);
    expect(overdue[0]).toMatchObject({
      rule: "tier2.refresh_overdue",
      severity: "WARNING",
      fingerprint: `tier2-refresh:${root}`,
    });
    expect(overdue[0].text).toContain("3 pending paths");
    expect(overdue.some((value) => value.rule === "index.stuck")).toBe(false);

    const previous = reconcile(overdue, {}, NOW + 1).next;
    for (const status of ["ready", "building"]) {
      const findings = detectIndexes(sample({ health: { metrics: { roots: [{
        project_root: root,
        tier2: { status, since_ms: NOW, last_progress_at_ms: NOW },
      }] } } }));
      expect(reconcile(findings, previous, NOW + 2).cleared.map((value) => value.fingerprint)).toEqual([
        `tier2-refresh:${root}`,
      ]);
    }
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
    expect(detectWakes(sample({ plugin_lines: ["event=bash_completion_wake_prompt_async_error session=ses_1 error=TypeError"] }))[0].rule).toBe("wakes.backlog");
  });

  test("a host permission line quoting the failure words is not a wake failure", () => {
    // The host log records every bash permission decision with the command text;
    // an operator grepping for the failure words must not read as a failure.
    const quoted = 'message=evaluated permission=bash pattern="grep -iE \\"this\\._client|promptAsync\\" aft-plugin.log"';
    const ok = "event=bash_completion_wake_prompt_async_ok session=ses_1";
    expect(detectWakes(sample({ plugin_lines: [quoted, ok] }))).toEqual([]);
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

  test("executor detector uses legacy maintenance fields when dispatch_liveness is absent", () => {
    const legacy = sample({ health: { metrics: { maintenance_inflight: 2, maintenance_queue_oldest_age_ms: 45_000, running_maintenance: 0 } } });
    const result = detectExecutor(legacy, { findings: {}, previous: { sampled_at_ms: NOW - 1, phantom_inflight: true } } as any);
    expect(result[0].rule).toBe("executor.phantom");
    expect(result[0].text).toContain("legacy fallback");
    const missing = detectExecutor(sample({ health: { metrics: {} } }), cleanState())[0];
    expect(missing.fingerprint).toBe("instrument:executor-health");
    expect(missing.text).toContain("waits on the sentinel health card");
  });

  test("health process_io prefers physical writes and falls back to logical writes", () => {
    expect(healthBytesWritten(sample({ health: { metrics: { process_io: { available: true, diskio_bytes_written: 42, logical_bytes_written: 99 } } } }))).toEqual({ available: true, bytes: 42 });
    expect(healthBytesWritten(sample({ health: { metrics: { process_io: { available: true, logical_bytes_written: 99 } } } }))).toEqual({ available: true, bytes: 99 });
    expect(healthBytesWritten(sample({ health: { metrics: { process_io: { available: false } } } }))).toEqual({ available: false });
  });

  test("write attribution ranks artifact growth and names unexplained WAL churn", () => {
    const input = sample({ disk: {
      free_bytes: 100 * 1024 ** 3,
      sizes: {},
      artifact_sizes: { "callgraph/aaa": 2.5 * 1024 ** 3, "inspect/bbb": 1.25 * 1024 ** 3, "semantic/ccc": 0.5 * 1024 ** 3, "views/ddd": 0.25 * 1024 ** 3 },
      artifact_roots: { "callgraph/aaa": "/root/a", "inspect/bbb": "/root/b", "semantic/ccc": "/root/c", "views/ddd": "/root/d" },
    } });
    const state: SentinelState = { findings: {}, previous: {
      sampled_at_ms: NOW - 120_000,
      artifact_sizes: { "callgraph/aaa": 2 * 1024 ** 3, "inspect/bbb": 1 * 1024 ** 3, "semantic/ccc": 0.4 * 1024 ** 3, "views/ddd": 0 },
    } };
    expect(writeGrowthAttribution(input, state, 4 * 1024 ** 3).split("\n").slice(1)).toEqual([
      "1. 0.50 GiB callgraph/aaa (/root/a) grew; 13% of write delta",
      "2. 0.25 GiB inspect/bbb (/root/b) grew; 6% of write delta",
      "3. 0.25 GiB views/ddd (/root/d) grew; 6% of write delta",
      "remainder: in-place rewrites (WAL churn)",
    ]);
  });

  test("write attribution shares use the census window and split its residual", () => {
    // The ledger rows cover the census's 10-minute window while the sentinel's
    // process interval can differ. Every share therefore uses the census total.
    const input = sample({ writes_census: {
      since_ms: NOW - 600_000,
      until_ms: NOW,
      process: { available: true, physical_bytes: 4 * 1024 ** 3 },
      attributed_physical_bytes: 3 * 1024 ** 3,
      unmeasurable_physical_bytes_estimate: 0.75 * 1024 ** 3,
      unexplained_physical_bytes: 0.25 * 1024 ** 3,
      unmeasurable: [{ seam: "db::TrackedConnection::drop" }],
      writers: [
        { domain: "callgraph_refresh", root_id: "/root/a", physical_bytes: 2 * 1024 ** 3 },
        { domain: "semantic_compaction", root_id: "/root/b", physical_bytes: 1024 ** 3 },
      ],
    } });
    const rendered = writeGrowthAttribution(input, cleanState(), 0.5 * 1024 ** 3);
    expect(rendered).toContain("callgraph_refresh (/root/a); 50% of the 10-minute window");
    expect(rendered).toContain("semantic_compaction (/root/b); 25% of the 10-minute window");
    expect(rendered).toContain("unmeasurable (estimate): 0.75 GiB; 19% of the 10-minute window; seams: db::TrackedConnection::drop");
    expect(rendered).toContain("unexplained: 0.25 GiB; 6% of the 10-minute window");
    expect(rendered).not.toContain("of write delta");
  });

  test("process footprint, cpu, and unexplained write rate are independent", () => {
    const state: SentinelState = { findings: {}, previous: { unexplained_write_rate_runs: 1 } };
    const writes_census = {
      since_ms: NOW - 3_600_000,
      until_ms: NOW,
      process: { physical_bytes: 2 * 1024 ** 3 },
      unexplained_physical_bytes: 2 * 1024 ** 3,
      writers: [],
    };
    expect(rules(detectProcess(sample({ process: { pid: 42, phys_footprint_bytes: 7 * 1024 ** 3, cpu_percent: 151 }, writes_census }), state))).toEqual(expect.arrayContaining(["process.footprint", "process.cpu", "process.writes"]));
  });

  test("one window over the unexplained-write ceiling is a burst, two consecutive is a regression", () => {
    const busy = {
      since_ms: NOW - 3_600_000,
      until_ms: NOW,
      process: { physical_bytes: 2 * 1024 ** 3 },
      unexplained_physical_bytes: 2 * 1024 ** 3,
      writers: [],
    };
    const firstWindow: SentinelState = { findings: {}, previous: {} };
    expect(rules(detectProcess(sample({ writes_census: busy }), firstWindow))).not.toContain("process.writes");

    const secondWindow: SentinelState = { findings: {}, previous: { unexplained_write_rate_runs: 1 } };
    const finding = detectProcess(sample({ writes_census: busy }), secondWindow).find((value) => value.rule === "process.writes");
    expect(finding?.text).toContain("unexplained physical write rate is 2.0 GiB/h");
    expect(finding?.clears_when).toContain("unexplained physical write rate");

    // A quiet window clears the streak, so a later burst starts over rather
    // than firing on the strength of an unrelated build an hour ago.
    const quiet = { ...busy, unexplained_physical_bytes: 0 };
    const priorHot: SentinelState = { findings: {}, previous: { unexplained_write_rate_runs: 1 } };
    expect(rules(detectProcess(sample({ writes_census: quiet }), priorHot))).not.toContain("process.writes");
  });

  test("an unavailable unexplained line raises an instrumentation finding", () => {
    const missingLine = detectProcess(sample({ writes_census: { since_ms: NOW - 600_000, until_ms: NOW } }), cleanState());
    expect(missingLine.find((value) => value.fingerprint === "instrument:writes-census")?.text).toContain("unexplained physical bytes");
    const failedCensus = detectProcess(sample({ writes_error: "profile failed" }), cleanState());
    expect(failedCensus.find((value) => value.fingerprint === "instrument:writes-census")?.text).toContain("profile failed");
  });

  test("tonight's 3.8 GiB per hour window does not alert when its residual is known-unmeasurable", () => {
    const writes_census = {
      since_ms: NOW - 3_600_000,
      until_ms: NOW,
      process: { physical_bytes: 3.8 * 1024 ** 3 },
      attributed_physical_bytes: 0.988 * 1024 ** 3,
      unmeasurable_physical_bytes_estimate: 2.812 * 1024 ** 3,
      unexplained_physical_bytes: 0,
      unmeasurable: [{ seam: "db::TrackedConnection::drop" }],
      writers: [],
    };
    const state: SentinelState = { findings: {}, previous: { unexplained_write_rate_runs: 1 } };
    expect(rules(detectProcess(sample({ writes_census }), state))).not.toContain("process.writes");
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

  test("dSYM detector distinguishes stale artifacts from missing keys", () => {
    const stale = detectDsym(sample({ dsym: { requested_uuid: "2CD06659", found_uuid: "E570EF4A", path: "/dsym/2CD06659/aft.dSYM" } }))[0];
    expect(stale.rule).toBe("dsym.stale");
    expect(stale.fingerprint).toBe("dsym:2CD06659");
    expect(stale.text).toContain("E570EF4A");
    expect(stale.text).toContain("re-stage");
    const missing = detectDsym(sample({ dsym: { requested_uuid: "2CD06659" } }))[0];
    expect(missing.rule).toBe("dsym.missing");
    expect(missing.fingerprint).toBe("dsym:2CD06659");
  });

  test("a scheduled workflow raises only once its failures become a streak", () => {
    // Nightly runs, newest first. The gate runs on a schedule and blocks no
    // merge, so nothing else in the toolchain reports these.
    const night = (day: number, conclusion: string, patch: Partial<ScheduledRun> = {}): ScheduledRun => ({
      workflow: "Nightly OSS cost gate",
      branch: "main",
      event: "schedule",
      status: "completed",
      conclusion,
      created_at: `2026-09-${String(day).padStart(2, "0")}T02:17:00Z`,
      run_id: 35000000000 + day,
      ...patch,
    });
    const green = night(15, "success");

    const oneRed = detectScheduledCi(sample({ ci_runs: [night(18, "failure"), night(17, "success"), green] }));
    expect(oneRed).toEqual([]);
    const twoRed = detectScheduledCi(sample({ ci_runs: [night(18, "failure"), night(17, "failure"), green] }));
    expect(twoRed).toEqual([]);

    const streak = detectScheduledCi(sample({ ci_runs: [night(18, "failure"), night(17, "timed_out"), night(16, "failure"), green] }))[0];
    expect(streak.rule).toBe("ci.scheduled_failing");
    expect(streak.severity).toBe("WARNING");
    expect(streak.fingerprint).toBe("ci:Nightly OSS cost gate");
    expect(streak.text).toContain("Nightly OSS cost gate");
    expect(streak.text).toContain("3 consecutive runs");
    expect(streak.text).toContain("since 2026-09-16");
    expect(streak.clears_when).toContain("succeeds");

    // One green night at the top ends the streak whatever came before it.
    const recovered = detectScheduledCi(sample({ ci_runs: [night(19, "success"), night(18, "failure"), night(17, "failure"), night(16, "failure")] }));
    expect(recovered).toEqual([]);
  });

  test("the streak counts scheduled main runs only, and is a lower bound when every listed run failed", () => {
    const row = (patch: Partial<ScheduledRun>): ScheduledRun => ({
      workflow: "Nightly OSS cost gate",
      branch: "main",
      event: "schedule",
      status: "completed",
      conclusion: "failure",
      created_at: "2026-09-18T02:17:00Z",
      ...patch,
    });
    // Each disqualifying axis gets a full streak's worth of reds, so dropping
    // any one of the three filters would raise a finding.
    const three = (patch: Partial<ScheduledRun>): ScheduledRun[] =>
      [18, 17, 16].map((day) => row({ ...patch, created_at: `2026-09-${day}T02:17:00Z` }));
    expect(detectScheduledCi(sample({ ci_runs: three({ event: "push" }) }))).toEqual([]);
    expect(detectScheduledCi(sample({ ci_runs: three({ branch: "release/0.57" }) }))).toEqual([]);
    expect(detectScheduledCi(sample({ ci_runs: three({ status: "in_progress", conclusion: null }) }))).toEqual([]);
    expect(detectScheduledCi(sample({ ci_runs: three({ conclusion: "cancelled" }) }))).toEqual([]);

    // A cancelled run is not a verdict about the workflow's subject, so it
    // neither extends the streak nor breaks it.
    const withCancel = [
      row({ created_at: "2026-09-18T02:17:00Z" }),
      row({ conclusion: "cancelled", created_at: "2026-09-17T02:17:00Z" }),
      row({ created_at: "2026-09-16T02:17:00Z" }),
      row({ created_at: "2026-09-15T02:17:00Z" }),
    ];
    const bounded = detectScheduledCi(sample({ ci_runs: withCancel }))[0];
    expect(bounded.text).toContain("at least 3 consecutive runs");
    expect(bounded.text).toContain("since 2026-09-15");
  });

  test("an unreadable run listing is an instrument finding, not silence", () => {
    const blind = detectScheduledCi(sample({ ci_error: "Error: gh: command not found" }))[0];
    expect(blind.rule).toBe("instrument");
    expect(blind.fingerprint).toBe("instrument:scheduled-ci");
    expect(blind.text).toContain("gh: command not found");
    // A sample that carries no CI state at all (a --specimen replay) stays quiet.
    expect(detectScheduledCi(sample())).toEqual([]);
  });

  test("dedupe alerts once, clears absent fingerprints, and stays quiet on a return inside the cooldown", () => {
    const current = detectStorage(sample({ disk: { free_bytes: 30 * 1024 ** 3, sizes: {} } }), cleanState())[0];
    const first = reconcile([current], {}, NOW);
    expect(first.raised).toHaveLength(1);
    const second = reconcile([current], first.next, NOW + 10 * 60_000);
    expect(second.raised).toHaveLength(0);
    const clear = reconcile([], second.next, NOW + 11 * 60_000);
    expect(clear.cleared.map((value) => value.fingerprint)).toEqual([current.fingerprint]);
    // A threshold the subject hovers around clears and returns every tick;
    // the return inside the cooldown must not wake anyone again, while each
    // clear still lands in the findings log.
    const back = reconcile([current], clear.next, NOW + 12 * 60_000);
    expect(back.raised).toHaveLength(0);
    const quiet = reconcile([], back.next, NOW + 13 * 60_000);
    expect(quiet.cleared).toHaveLength(1);
    expect(reconcile([], quiet.next, NOW + 14 * 60_000).cleared).toHaveLength(0);
    expect(reconcile([current], quiet.next, NOW + 3 * 60 * 60_000).raised).toHaveLength(1);
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

describe("daemon pid discovery", () => {
  test("the newest log is not the daemon when a CLI process wrote it last", () => {
    const dir = mkdtempSync(join(tmpdir(), "aft-sentinel-pid-"));
    writeFileSync(join(dir, "aft-74022.log"), "daemon\n");
    writeFileSync(join(dir, "aft-171.log"), "cli probe\n");
    const later = Date.now() / 1000 + 5;
    utimesSync(join(dir, "aft-171.log"), later, later);
    const ps = (pid: number) => (pid === 74022 ? "/Users/x/.local/share/cortexkit/bin/ck-aft --subc /Users/x/run/subc-connection.json" : pid === 171 ? "/Users/x/aft/target/debug/aft" : "");
    expect(daemonPidLog(dir, ps).pid).toBe(74022);
  });
  test("no live daemon among the logs is an error, not a pid", () => {
    const dir = mkdtempSync(join(tmpdir(), "aft-sentinel-pid-"));
    writeFileSync(join(dir, "aft-171.log"), "cli probe\n");
    expect(() => daemonPidLog(dir, () => "/Users/x/aft/target/debug/aft")).toThrow(/no aft pid log belongs to a running --subc daemon/);
  });
  test("isSubcDaemon requires the aft binary and the --subc flag", () => {
    expect(isSubcDaemon(1, () => "/opt/bin/ck-aft --subc /run/c.json")).toBe(true);
    expect(isSubcDaemon(1, () => "/opt/bin/ck-aft profile --writes")).toBe(false);
    expect(isSubcDaemon(1, () => "/usr/libexec/other --subc x")).toBe(false);
    expect(isSubcDaemon(1, () => "")).toBe(false);
  });
});
