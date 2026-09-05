import { spawn, spawnSync } from "node:child_process";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  npmInvocation,
  npmSpawnEnv,
  type ResolvedNpm,
  resolveNpm,
  terminateNpmProcessTree,
} from "@cortexkit/aft-bridge";

import { OpenCodeAdapter } from "../adapters/opencode.js";
import type { HarnessAdapter } from "../adapters/types.js";
import { diagnoseOpenCodeLoad } from "../doctor/opencode.js";
import { type AftResponse, sendAftRequest } from "../lib/aft-bridge.js";
import { getBinaryCacheInfo } from "../lib/binary-cache.js";
import { findAftBinary, probeAftBinary } from "../lib/binary-probe.js";
import { buildRecentAftToolFailuresSectionFromLog } from "../lib/bridge-tool-failures.js";
import {
  DOCTOR_BUILD_BREAKER_RESET_COMMAND,
  formatBuildBreakerSuspension,
  resetBuildBreakerSuspension,
} from "../lib/build-breaker.js";
import { CLI } from "../lib/cli.js";
import {
  collectDiagnosticIssues,
  collectDiagnostics,
  type DiagnosticReport,
  findPluginCliVersionSkews,
  formatDiagnosticIssuesSection,
  renderDiagnosticsMarkdown,
  tailLogFile,
} from "../lib/diagnostics.js";
import { dirSize, formatBytes } from "../lib/fs-util.js";
import { createGitHubIssue, isGhInstalled, openBrowser } from "../lib/github.js";
import { resolveAdaptersForCommand } from "../lib/harness-select.js";
import {
  capBodyToGithubLimit,
  extractRecentErrors,
  filterLogToSession,
} from "../lib/issue-body.js";
import {
  AFT_SCHEMA_URL,
  ensureAftSchemaUrl,
  type JsoncFormat,
  readJsoncFile,
} from "../lib/jsonc.js";
import { type ClearResult, clearLspCaches } from "../lib/lsp-cache.js";
import { findOnnxFixCandidates, runOnnxFix } from "../lib/onnx-fix.js";
import { confirm, intro, log, note, outro, selectMany, selectOne, text } from "../lib/prompts.js";
import { sanitizeContent } from "../lib/sanitize.js";
import { getSelfVersion } from "../lib/self-version.js";
import { listRecentSessions, type RecentSession, truncateTitle } from "../lib/sessions.js";
import { formatHostGenerations, type OpenCodeHostDetection } from "../setup/host-generation.js";

export type DoctorClearTarget = "plugin-cache" | "lsp-cache" | "binary-cache";

export const DOCTOR_CLEAR_TARGET_OPTIONS: { label: string; value: DoctorClearTarget }[] = [
  {
    label: "Plugin npm cache (~/.cache/opencode/packages/@cortexkit/aft-opencode@latest, etc.)",
    value: "plugin-cache",
  },
  {
    label: "LSP install cache (~/.cache/aft/lsp-packages/, ~/.cache/aft/lsp-binaries/)",
    value: "lsp-cache",
  },
  {
    label: "Old aft binaries (~/.cache/aft/bin/v* — keeps the version matching this CLI)",
    value: "binary-cache",
  },
];

export const DOCTOR_FORCE_CLEAR_TARGETS: DoctorClearTarget[] = ["plugin-cache"];

export interface DoctorOptions {
  clear: boolean;
  fix: boolean;
  force: boolean;
  issue: boolean;
  argv: string[];
  /** Optional adapter override lets tests render doctor output without launching a host. */
  resolveAdapters?: typeof resolveAdaptersForCommand;
  collectDiagnostics?: typeof collectDiagnostics;
  collectRemovalHealth?: typeof collectRemovalHealth;
  detectOpenCodeHost?: () => OpenCodeHostDetection;
}

function openCodeAdapter(adapters: HarnessAdapter[]): HarnessAdapter | undefined {
  return adapters.find((adapter) => adapter.kind === "opencode");
}

function detectOpenCodeForCommand(
  adapters: HarnessAdapter[],
  override?: () => OpenCodeHostDetection,
): OpenCodeHostDetection | null {
  const adapter = openCodeAdapter(adapters);
  if (!adapter) return null;
  if (override) return override();
  return adapter instanceof OpenCodeAdapter ? adapter.detectHostGeneration() : null;
}

function refuseAmbiguousOpenCodeWrites(detection: OpenCodeHostDetection | null): boolean {
  if (detection?.status !== "ambiguous") return false;
  log.error(
    "OpenCode: host generation ambiguous (V1, V2); refusing configuration writes until only one generation is selected on PATH.",
  );
  return true;
}

function adapterConfigNeedsUpdate(adapter: HarnessAdapter, tui = false): boolean {
  const method = tui ? "needsTuiPluginEntryUpdate" : "needsPluginEntryUpdate";
  const candidate = adapter as HarnessAdapter & Partial<Record<typeof method, () => boolean>>;
  return candidate[method]?.() ?? false;
}

export interface RemovalHealth {
  available: boolean;
  usageWindowDays?: number;
  projectRootsServed?: number;
  sessionsServed?: number;
  projectRootsSource?: string;
  runningBackgroundTasks?: number;
  undoHistorySessions?: number;
  message?: string;
}

export interface CacheClearSummary {
  hadErrors: boolean;
  pluginCache?: {
    cleared: number;
    totalBytes: number;
    errors: number;
  };
  lspCache?: {
    cleared: number;
    totalBytes: number;
    errors: number;
  };
  binaryCache?: {
    cleared: number;
    totalBytes: number;
    errors: number;
  };
}

export interface CacheClearOptions {
  clearLspCaches?: () => ClearResult;
  includePluginBytes?: boolean;
}

/** Build native arguments for `doctor --profile [seconds]` without changing profile semantics. */
export function buildDoctorProfileArgs(argv: string[]): string[] {
  const profileIndex = argv.indexOf("--profile");
  const profileArgs = argv.slice(profileIndex + 1);
  const seconds = profileArgs[0];
  if (seconds && /^\d+$/.test(seconds)) {
    return ["profile", "--seconds", seconds, ...profileArgs.slice(1)];
  }
  return ["profile", ...profileArgs];
}

/** Run the native sampler directly so doctor does not add its normal diagnostics output. */
export function runDoctorProfile(argv: string[]): number {
  const binary = findAftBinary();
  if (!binary) {
    console.error(
      "aft doctor --profile requires a native AFT binary; run `aft doctor --fix` first.",
    );
    return 1;
  }
  const result = spawnSync(binary, buildDoctorProfileArgs(argv), {
    stdio: "inherit",
    env: process.env,
  });
  if (result.error) {
    console.error(`aft profile failed to start ${binary}: ${result.error.message}`);
    return 1;
  }
  return result.status ?? 1;
}

export async function runDoctor(options: DoctorOptions): Promise<number> {
  if (options.issue) {
    return runIssueFlow(options.argv);
  }
  intro(`${CLI} doctor`);

  if (options.fix) {
    return runFixFlow(
      options.argv,
      options.detectOpenCodeHost,
      options.resolveAdapters,
      options.collectDiagnostics,
    );
  }

  if (options.clear) {
    return runClearFlow(options.argv);
  }

  const resolveAdapters = options.resolveAdapters ?? resolveAdaptersForCommand;
  const adapters = await resolveAdapters(options.argv, {
    allowMulti: false,
    verb: "diagnose",
  });

  const hostDetection = detectOpenCodeForCommand(adapters, options.detectOpenCodeHost);
  const report = await (options.collectDiagnostics ?? collectDiagnostics)(adapters);
  const removalHealth = await (options.collectRemovalHealth ?? collectRemovalHealth)(adapters);
  const opencodeAdapter = openCodeAdapter(adapters);
  const opencodeHarness = report.harnesses.find((harness) => harness.kind === "opencode");
  const opencodeDoctor =
    hostDetection && opencodeAdapter && opencodeHarness
      ? diagnoseOpenCodeLoad({
          detection: hostDetection,
          configPath: opencodeHarness.configPaths.harnessConfig,
          logPath: opencodeHarness.logFile.path,
          pluginCachePath: opencodeHarness.pluginCache.path,
          cachedPluginVersion: opencodeHarness.pluginCache.cached,
          expectedPluginEntry: opencodeAdapter.pluginEntryWithVersion,
        })
      : null;

  log.info(`AFT CLI v${report.cliVersion}, AFT binary ${report.binaryVersion ?? "unknown"}`);
  if (!report.binaryVersion) {
    const hasEnabledRegisteredHarness = report.harnesses.some(
      (h) => h.pluginRegistered && h.aftConfig.enabled,
    );
    const hasRegisteredHarness = report.harnesses.some((h) => h.pluginRegistered);
    const binaryIssue = collectDiagnosticIssues(report).find(
      (issue) => issue.code === "binary_missing",
    );
    if (hasEnabledRegisteredHarness && binaryIssue?.severity === "info") {
      log.info(
        `  no matching aft binary detected — it will self-install when the next AFT-enabled session starts (or run \`${CLI} doctor --fix\`)`,
      );
    } else if (hasEnabledRegisteredHarness) {
      log.warn(
        `  no matching aft binary detected — run \`${CLI} doctor --fix\` to download, or start an AFT-enabled session to trigger plugin-side install`,
      );
    } else if (hasRegisteredHarness) {
      log.info(
        "  no matching aft binary detected; all registered AFT harnesses are disabled by config",
      );
    } else {
      log.warn(`  no matching aft binary detected — run \`${CLI} doctor --fix\` to download`);
    }
    logUnmatchedBinaryCandidates(report.cliVersion);
  }
  log.info(
    `Binary cache: ${report.binaryCache.versions.length} version(s), ${formatBytes(report.binaryCache.totalSize)} at ${report.binaryCache.path}`,
  );
  logBuildBreakerSuspensions(report);

  log.step("If you remove AFT");
  for (const line of renderRemovalSection(removalHealth)) {
    log.info(`  ${line}`);
  }

  const npmCount = report.lspCache.npm.entries.length;
  const ghCount = report.lspCache.github.entries.length;
  if (npmCount + ghCount > 0) {
    log.info(
      `LSP cache: ${npmCount} npm + ${ghCount} github install(s), ${formatBytes(report.lspCache.totalSize)} total`,
    );
  }

  const hadProblems = hasDoctorProblems(report) || Boolean(opencodeDoctor?.problems.length);
  for (const h of report.harnesses) {
    log.step(`${h.displayName}`);
    if (!h.hostInstalled) {
      log.warn(`  host not installed — install from: ${describeAdapterInstallHint(h.kind)}`);
      continue;
    }
    log.info(`  host: ${h.hostVersion ?? "unknown version"}`);
    if (h.kind === "opencode" && hostDetection) {
      log.info(`  host generation: ${formatHostGenerations(hostDetection)}`);
      if (opencodeDoctor?.takenLoadPath) {
        log.info(`  load path: ${opencodeDoctor.takenLoadPath}`);
      }
      if (
        opencodeDoctor?.expectedLoadPath &&
        opencodeDoctor.takenLoadPath !== opencodeDoctor.expectedLoadPath
      ) {
        log.error(`  expected load path: ${opencodeDoctor.expectedLoadPath}`);
      }
      for (const problem of opencodeDoctor?.problems ?? []) log.error(`  ${problem}`);
    }
    log.info(`  plugin registered: ${h.pluginRegistered ? "yes" : "no"}`);
    log.info(
      `  plugin version: ${h.kind === "opencode" ? (opencodeDoctor?.pluginVersion ?? h.pluginCache.cached ?? "not installed") : (h.pluginCache.cached ?? "not installed")}`,
    );
    if (!h.aftConfig.enabled) {
      log.info(
        `  AFT disabled by config${h.aftConfig.enabledSource ? ` (${h.aftConfig.enabledSource})` : ""}; plugin will stay inert`,
      );
    }
    if (!h.pluginRegistered) {
      log.warn(
        `  plugin registration can be fixed with \`${CLI} setup\` or \`${CLI} doctor --fix\``,
      );
    }

    log.info(`  aft config: ${h.aftConfig.exists ? h.configPaths.aftConfig : "(not set)"}`);
    if (h.aftConfig.parseError) {
      log.error(`  aft config parse error: ${h.aftConfig.parseError}`);
    } else if (h.aftConfig.exists) {
      const { value } = readJsoncFile(h.configPaths.aftConfig);
      const schemaSet = value?.$schema === AFT_SCHEMA_URL;
      log.info(
        `  aft config $schema: ${schemaSet ? "set" : `not set — run \`${CLI} doctor --fix\` for editor autocomplete`}`,
      );
    }

    log.info(`  storage: ${formatDoctorStorageStatus(h)}`);

    if (h.onnxRuntime.required) {
      const parts: string[] = [];
      parts.push(`required: yes (${h.onnxRuntime.platform})`);
      if (h.onnxRuntime.cachedPath) {
        parts.push(
          `cached: ${h.onnxRuntime.cachedVersion ?? "unknown"}${h.onnxRuntime.cachedCompatible === false ? " (incompatible)" : ""}`,
        );
      }
      if (h.onnxRuntime.systemPath) {
        parts.push(
          `system: ${h.onnxRuntime.systemVersion ?? "unknown"}${h.onnxRuntime.systemCompatible === false ? " (incompatible)" : ""}`,
        );
      } else if (h.onnxRuntime.ignoredSystemPath) {
        parts.push(
          `system: ${h.onnxRuntime.ignoredSystemReason} at ${h.onnxRuntime.ignoredSystemPath}`,
        );
      }
      if (!h.onnxRuntime.cachedPath && !h.onnxRuntime.systemPath) {
        parts.push(`not installed — ${h.onnxRuntime.installHint}`);
      }
      if (h.onnxRuntime.cachedCompatible === false || h.onnxRuntime.systemCompatible === false) {
        parts.push(`needs reinstall — run \`${CLI} doctor --fix\``);
      }
      log.info(`  onnx runtime: ${parts.join(" · ")}`);
    } else {
      log.info("  onnx runtime: not required (semantic search disabled; ignoring ONNX status)");
    }

    log.info(
      `  log: ${h.logFile.exists ? `${h.logFile.path} (${h.logFile.sizeKb} KB)` : "(not written yet)"}`,
    );
  }

  // Compatibility: `doctor --force` only clears the plugin package cache.
  // Plain `doctor` must remain strictly read-only: plugin registration is only
  // mutated by `aft setup` or the explicit `aft doctor --fix` flow.
  if (options.force) {
    if (refuseAmbiguousOpenCodeWrites(hostDetection)) return 1;
    await clearDoctorCaches(adapters, DOCTOR_FORCE_CLEAR_TARGETS, { includePluginBytes: false });
  }

  if (hadProblems) {
    logDoctorIssues(report);
    note(
      `Run \`${CLI} setup\` or \`${CLI} doctor --fix\` to register AFT with any harness showing \`plugin registered: no\`. Run \`${CLI} doctor --fix\` for ONNX Runtime issues or to download a missing aft binary.`,
      "Tips",
    );
    outro("Done — some issues found.");
    return 1;
  }
  outro("Everything looks good.");
  return 0;
}

async function collectRemovalHealth(adapters: HarnessAdapter[]): Promise<RemovalHealth> {
  const adapter = adapters[0];
  if (!adapter) {
    return { available: false, message: "no harness storage root was selected" };
  }

  const binary = findAftBinary(getSelfVersion());
  if (!binary) {
    return { available: false, message: "the aft binary could not be found" };
  }

  try {
    const response = await sendAftRequest(binary, {
      id: "doctor-removal-status",
      command: "status",
      // The Rust status handler opens this exact database read-only. Doctor must
      // not configure a bridge just to inspect what removal would leave behind.
      removal_storage_dir: adapter.getStorageDir(),
    });
    return coerceRemovalHealth(response);
  } catch (error) {
    return {
      available: false,
      message: error instanceof Error ? error.message : String(error),
    };
  }
}

function coerceRemovalHealth(response: AftResponse): RemovalHealth {
  if (!response.success) {
    return { available: false, message: response.message ?? response.code ?? "status failed" };
  }
  const raw = response.removal;
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
    return { available: false, message: "the aft binary did not return removal state" };
  }
  const removal = raw as Record<string, unknown>;
  if (removal.available !== true) {
    return {
      available: false,
      message:
        typeof removal.message === "string" ? removal.message : "removal state is unavailable",
    };
  }

  const usageWindowDays = nonNegativeInteger(removal.usage_window_days);
  const projectRootsServed = nonNegativeInteger(removal.project_roots_served);
  const sessionsServed = nonNegativeInteger(removal.sessions_served);
  const runningBackgroundTasks = nonNegativeInteger(removal.running_background_tasks);
  const undoHistorySessions = nonNegativeInteger(removal.undo_history_sessions);
  if (
    usageWindowDays === null ||
    projectRootsServed === null ||
    sessionsServed === null ||
    runningBackgroundTasks === null ||
    undoHistorySessions === null ||
    typeof removal.project_roots_source !== "string"
  ) {
    return { available: false, message: "the aft binary returned incomplete removal state" };
  }

  return {
    available: true,
    usageWindowDays,
    projectRootsServed,
    sessionsServed,
    projectRootsSource: removal.project_roots_source,
    runningBackgroundTasks,
    undoHistorySessions,
  };
}

function nonNegativeInteger(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

/** Render AFT's durable removal costs; the section is shown even when state is unavailable. */
export function renderRemovalSection(health: RemovalHealth): string[] {
  if (!health.available) {
    return [`usage and deferred work: unavailable (${health.message ?? "unknown error"})`];
  }

  const usageWindowDays = health.usageWindowDays ?? 7;
  const projectRootsServed = health.projectRootsServed ?? 0;
  const sessionsServed = health.sessionsServed ?? 0;
  const runningBackgroundTasks = health.runningBackgroundTasks ?? 0;
  const undoHistorySessions = health.undoHistorySessions ?? 0;

  return [
    `last ${usageWindowDays} days: ${projectRootsServed} ${plural(projectRootsServed, "project root")} served (approx. from durable task/backup project keys; root paths are not retained)`,
    `last ${usageWindowDays} days: ${sessionsServed} ${plural(sessionsServed, "session")} served (durable task/backup activity)`,
    runningBackgroundTasks === 0
      ? "no running tasks"
      : `${runningBackgroundTasks} running background ${plural(runningBackgroundTasks, "task")} would orphan`,
    undoHistorySessions === 0
      ? "no undo history recorded"
      : `undo history for ${undoHistorySessions} ${plural(undoHistorySessions, "session")} becomes unreachable (files themselves are untouched)`,
  ];
}

function plural(count: number, singular: string): string {
  return count === 1 ? singular : `${singular}s`;
}

export function hasDoctorProblems(report: DiagnosticReport): boolean {
  // GitHub #46 follow-up: an absent aft binary is a real problem the user
  // should see flagged, not buried under a misleading "Everything looks
  // good." outro. Reproducer: `rm -rf ~/.cache/aft/bin && bunx --bun
  // @cortexkit/aft doctor` previously printed "AFT binary unknown" + "Binary
  // cache: 0 versions" and then "Everything looks good." at the bottom.
  return (
    collectDiagnosticIssues(report).some((issue) => issue.severity !== "info") ||
    (report.buildBreakerSuspensions?.length ?? 0) > 0
  );
}

export function logBuildBreakerSuspensions(report: DiagnosticReport): void {
  for (const suspension of report.buildBreakerSuspensions ?? []) {
    log.warn(formatBuildBreakerSuspension(suspension));
  }
}

export async function runDoctorBuildBreakerReset(argv: string[]): Promise<number> {
  const optionValue = (name: string): string | undefined => {
    const index = argv.indexOf(name);
    return index >= 0 ? argv[index + 1] : undefined;
  };
  const root = optionValue("--root");
  const domain = optionValue("--domain");
  const fingerprint = optionValue("--fingerprint");
  if (!root || !domain || !fingerprint) {
    log.error(
      `Usage: ${DOCTOR_BUILD_BREAKER_RESET_COMMAND} --root <root> --domain <domain> --fingerprint <fingerprint>`,
    );
    return 2;
  }

  const adapters = await resolveAdaptersForCommand(argv, {
    allowMulti: false,
    verb: "reset the build breaker for",
  });
  const storageRoot = adapters[0]?.getStorageDir();
  if (!storageRoot) {
    log.error("No AFT storage root was found for the selected harness.");
    return 1;
  }
  const reset = resetBuildBreakerSuspension(storageRoot, { root, domain, fingerprint });
  if (reset === 0) {
    log.warn("No matching build-breaker suspension was found; no records were changed.");
    return 1;
  }
  log.success(`Reset build breaker for root=${root} domain=${domain}.`);
  return 0;
}

async function runClearFlow(argv: string[]): Promise<number> {
  const targets = await selectMany<DoctorClearTarget>(
    "What do you want to clear?",
    DOCTOR_CLEAR_TARGET_OPTIONS,
    undefined,
    false,
  );

  if (targets.length === 0) {
    log.info("No cache categories selected; nothing to clear.");
    outro("Done.");
    return 0;
  }

  const adapters = targets.includes("plugin-cache")
    ? await resolveAdaptersForCommand(argv, {
        allowMulti: true,
        verb: "clear plugin cache for",
      })
    : [];

  const summary = await clearDoctorCaches(adapters, targets);
  outro(summary.hadErrors ? "Done — some cache entries could not be cleared." : "Done.");
  return summary.hadErrors ? 1 : 0;
}

export async function clearDoctorCaches(
  adapters: HarnessAdapter[],
  targets: readonly DoctorClearTarget[],
  options: CacheClearOptions = {},
): Promise<CacheClearSummary> {
  const summary: CacheClearSummary = { hadErrors: false };

  if (targets.includes("plugin-cache")) {
    let cleared = 0;
    let totalBytes = 0;
    let errors = 0;

    for (const adapter of adapters) {
      const result = await clearPluginCache(adapter, options.includePluginBytes ?? true);
      if (result.action === "cleared") {
        cleared += 1;
        totalBytes += result.bytes;
      } else if (result.action === "error") {
        errors += 1;
        summary.hadErrors = true;
      }
    }

    summary.pluginCache = { cleared, totalBytes, errors };
  }

  if (targets.includes("lsp-cache")) {
    const cleanup = (options.clearLspCaches ?? clearLspCaches)();
    reportLspCacheClear(cleanup);
    if (cleanup.errors.length > 0) {
      summary.hadErrors = true;
    }
    summary.lspCache = {
      cleared: cleanup.cleared.length,
      totalBytes: cleanup.totalBytes,
      errors: cleanup.errors.length,
    };
  }

  if (targets.includes("binary-cache")) {
    const result = clearOldBinaries();
    if (result.errors.length > 0) {
      summary.hadErrors = true;
    }
    summary.binaryCache = {
      cleared: result.cleared,
      totalBytes: result.bytesReclaimed,
      errors: result.errors.length,
    };
  }

  return summary;
}

/**
 * Clear cached `aft` binaries except the version this CLI ships with.
 *
 * Each release of `@cortexkit/aft` bundles a matching binary version; the
 * plugin downloads it on first use into `~/.cache/aft/bin/v<version>/aft`.
 * Older versions are kept around for rollback and to handle the
 * "old plugin instance still running" scenario, but they pile up over
 * time and a single binary is ~30 MB on macOS / Linux. Clearing keeps
 * the version that matches the running CLI so we don't yank the binary
 * a live OpenCode/Pi process is currently executing from.
 */
export interface BinaryCacheClearResult {
  cleared: number;
  bytesReclaimed: number;
  errors: { path: string; error: string }[];
  keptVersion: string | null;
}

export function clearOldBinaries(): BinaryCacheClearResult {
  // Keep the version that matches the running CLI. Different release
  // tags share a `v` prefix; binaries on disk follow the same shape.
  const cliVersion = getSelfVersion();
  const keepTag = `v${cliVersion.replace(/^v/, "")}`;
  const info = getBinaryCacheInfo(cliVersion);
  const result: BinaryCacheClearResult = {
    cleared: 0,
    bytesReclaimed: 0,
    errors: [],
    keptVersion: keepTag,
  };

  if (!existsSync(info.path)) {
    log.info(`Binary cache: nothing to clear at ${info.path}`);
    return result;
  }

  const stale = info.versions.filter((v) => v !== keepTag);

  if (stale.length === 0) {
    log.info(
      `Binary cache: only the active version (${keepTag}) is present at ${info.path}; nothing to clear`,
    );
    return result;
  }

  for (const version of stale) {
    const dir = join(info.path, version);
    let bytes = 0;
    try {
      bytes = statSync(dir).isDirectory() ? dirSize(dir) : 0;
    } catch {
      bytes = 0;
    }
    try {
      rmSync(dir, { recursive: true, force: true });
      result.cleared += 1;
      result.bytesReclaimed += bytes;
      log.success(`Binary cache: cleared ${dir} (reclaimed ${formatBytes(bytes)})`);
    } catch (err) {
      const message = (err as Error).message ?? "unknown error";
      log.error(`Binary cache: failed to remove ${dir}: ${message}`);
      result.errors.push({ path: dir, error: message });
    }
  }

  if (result.cleared > 0) {
    log.success(
      `Binary cache: kept ${keepTag}, removed ${result.cleared} old version(s), reclaimed ${formatBytes(result.bytesReclaimed)}`,
    );
  }

  return result;
}

export interface DoctorFixPlanItem {
  kind: "plugin" | "plugin-update" | "binary" | "onnx" | "storage" | "schema";
  message: string;
}

/**
 * Harnesses whose installed plugin is OLDER than this CLI (the
 * `plugin_cli_version_skew` diagnostic). This is the case where the plugin's own
 * auto-updater couldn't run `npm install` (commonly because a GUI/Desktop launch
 * had no npm on PATH), so the user is stuck on the old plugin. `doctor --fix`
 * can reinstall the latest plugin via the npm we resolve beyond PATH.
 */
interface PluginUpdateTarget {
  adapter: HarnessAdapter;
  installDir: string;
  cached: string;
  latest: string;
}

function findPluginUpdateTargets(
  adapters: HarnessAdapter[],
  report: DiagnosticReport,
): PluginUpdateTarget[] {
  const adaptersByKind = new Map(adapters.map((a) => [a.kind, a]));
  const targets: PluginUpdateTarget[] = [];
  for (const harness of report.harnesses) {
    // Only OpenCode reinstalls a plugin npm package this way; Pi manages its own
    // packages via `pi install`, handled by the plugin-registration fix.
    if (harness.kind !== "opencode") continue;
    if (!harness.aftConfig.enabled) continue;
    if (!harness.hostInstalled || !harness.pluginRegistered) continue;
    const cache = harness.pluginCache;
    if (!cache?.exists || !cache.cached || !cache.latest) continue;
    if (cache.cached === cache.latest) continue;
    const adapter = adaptersByKind.get(harness.kind);
    if (!adapter) continue;
    targets.push({
      adapter,
      installDir: cache.path,
      cached: cache.cached,
      latest: cache.latest,
    });
  }
  return targets;
}

interface SchemaFixTarget {
  adapter: HarnessAdapter;
  aftConfig: string;
  aftConfigFormat: JsoncFormat;
}

/**
 * Installed harnesses whose AFT config is missing the `$schema` URL (so editor
 * autocomplete/validation won't kick in). `aft setup` already sets this; this
 * is the `--fix` counterpart for configs created before setup or hand-edited.
 * Plain `aft doctor` stays read-only and only reports it.
 */
function findSchemaFixTargets(adapters: HarnessAdapter[]): SchemaFixTarget[] {
  const targets: SchemaFixTarget[] = [];
  for (const adapter of adapters) {
    if (!adapter.isInstalled()) continue;
    let aftConfig: string;
    let aftConfigFormat: JsoncFormat;
    try {
      ({ aftConfig, aftConfigFormat } = adapter.detectConfigPaths());
    } catch {
      continue;
    }
    const { value } = readJsoncFile(aftConfig);
    if (value?.$schema === AFT_SCHEMA_URL) continue;
    targets.push({ adapter, aftConfig, aftConfigFormat });
  }
  return targets;
}

async function runDoctorNpmInstall(npm: ResolvedNpm, installDir: string): Promise<void> {
  const invocation = npmInvocation(npm, [
    "install",
    "--no-audit",
    "--no-fund",
    "--no-progress",
    "--ignore-scripts",
  ]);
  await new Promise<void>((resolve, reject) => {
    const child = spawn(invocation.command, invocation.args, {
      cwd: installDir,
      env: { ...npmSpawnEnv(npm), ...invocation.env },
      stdio: ["ignore", "ignore", "pipe"],
      windowsVerbatimArguments: invocation.windowsVerbatimArguments,
    });
    let stderr = "";
    let settled = false;
    let terminating = false;
    let timeout: ReturnType<typeof setTimeout> | null = null;
    const finish = (error?: Error) => {
      if (settled) return;
      settled = true;
      if (timeout) clearTimeout(timeout);
      if (error) reject(error);
      else resolve();
    };
    child.stderr?.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
      if (stderr.length > 16 * 1024) stderr = stderr.slice(-16 * 1024);
    });
    child.once("error", (error) => {
      if (terminating) return;
      finish(error);
    });
    child.once("exit", (code) => {
      if (terminating) return;
      const detail = stderr.trim();
      if (code !== 0) {
        finish(new Error(`npm install exited with code ${code}${detail ? `: ${detail}` : ""}`));
      } else {
        finish();
      }
    });
    timeout = setTimeout(() => {
      if (settled) return;
      terminating = true;
      void terminateNpmProcessTree(child, invocation).then(
        () => finish(new Error("npm install timed out after 120000ms")),
        (error) =>
          finish(
            new Error(`npm install timed out and termination outcome is unknown: ${String(error)}`),
          ),
      );
    }, 120_000);
  });
}

async function applyPluginUpdates(
  targets: PluginUpdateTarget[],
): Promise<{ updated: number; errors: number }> {
  let updated = 0;
  let errors = 0;
  if (targets.length === 0) return { updated, errors };

  const npm = resolveNpm();
  if (!npm) {
    errors += targets.length;
    log.error(
      "Could not find npm on PATH or in known version-manager locations, so the plugin cannot be updated automatically. Install Node/npm, or launch your editor from a shell where npm is available.",
    );
    return { updated, errors };
  }

  for (const target of targets) {
    try {
      // `npm install` in the plugin's cache dir reinstalls against the
      // package.json dependency spec OpenCode wrote (pinned to @latest), pulling
      // the newest plugin. Mirrors the plugin auto-updater's install flags.
      await runDoctorNpmInstall(npm, target.installDir);
      updated += 1;
      log.success(
        `${target.adapter.displayName}: plugin updated ${target.cached} → ${target.latest} (restart ${target.adapter.displayName} to apply)`,
      );
    } catch (err) {
      errors += 1;
      const message = err instanceof Error ? err.message : String(err);
      log.error(`${target.adapter.displayName}: plugin update failed: ${message}`);
    }
  }
  return { updated, errors };
}

function applySchemaFixes(targets: SchemaFixTarget[]): { changed: number; errors: number } {
  let changed = 0;
  let errors = 0;
  for (const target of targets) {
    try {
      const result = ensureAftSchemaUrl(target.aftConfig, target.aftConfigFormat);
      if (result.action === "added" || result.action === "updated") {
        changed += 1;
        log.success(`${target.adapter.displayName}: ${result.message}`);
      }
    } catch (error) {
      errors += 1;
      log.warn(
        `${target.adapter.displayName}: could not set $schema on ${target.aftConfig}: ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
    }
  }
  return { changed, errors };
}

export function buildDoctorFixPlan(
  adapters: HarnessAdapter[],
  report: DiagnosticReport,
): DoctorFixPlanItem[] {
  const items: DoctorFixPlanItem[] = [];
  const adaptersByKind = new Map<string, HarnessAdapter>(
    adapters.map((adapter) => [adapter.kind, adapter]),
  );

  for (const harness of report.harnesses) {
    const adapter = adaptersByKind.get(harness.kind);
    if (!adapter || !harness.hostInstalled) continue;
    const needsPluginConfig = !harness.pluginRegistered || adapterConfigNeedsUpdate(adapter);
    if (!needsPluginConfig) continue;
    if (adapter.kind === "pi") {
      items.push({
        kind: "plugin",
        message: `Will run \`pi install ${adapter.pluginEntryWithVersion}\` to register ${adapter.displayName}`,
      });
    } else {
      items.push({
        kind: "plugin",
        message: `Will ${harness.pluginRegistered ? "update" : "add"} ${adapter.pluginEntryWithVersion} ${harness.pluginRegistered ? "in" : "to"} ${harness.configPaths.harnessConfig}`,
      });
    }
  }

  for (const harness of report.harnesses) {
    const adapter = adaptersByKind.get(harness.kind);
    if (!adapter || !harness.hostInstalled) continue;
    if (!adapter.ensureTuiPluginEntry || !adapter.hasTuiPluginEntry) continue;
    const hasTuiEntry = adapter.hasTuiPluginEntry();
    if (hasTuiEntry && !adapterConfigNeedsUpdate(adapter, true)) continue;
    items.push({
      kind: "plugin",
      message: `Will ${hasTuiEntry ? "update" : "add"} ${adapter.pluginEntryWithVersion} ${hasTuiEntry ? "in" : "to"} ${harness.configPaths.tuiConfig} (TUI sidebar)`,
    });
  }

  for (const target of findPluginUpdateTargets(adapters, report)) {
    items.push({
      kind: "plugin-update",
      message: `Will update ${target.adapter.displayName} plugin ${target.cached} → ${target.latest} via npm (the plugin's own auto-update could not run, often no npm on PATH)`,
    });
  }

  const hasEnabledHarness = report.harnesses.some((harness) => {
    return harness.hostInstalled && harness.aftConfig.enabled;
  });
  if (!report.binaryVersion && hasEnabledHarness) {
    const skews = findPluginCliVersionSkews(report);
    items.push({
      kind: "binary",
      message:
        skews.length > 0
          ? `Will ask before caching CLI v${report.cliVersion} because the installed plugin will not use it until updated`
          : `Will download/cache the aft binary matching CLI v${report.cliVersion}`,
    });
  }

  for (const harness of report.harnesses) {
    if (!harness.hostInstalled || !harness.pluginRegistered || !harness.aftConfig.enabled) continue;
    if (harness.storageDir.exists) continue;
    items.push({
      kind: "storage",
      message: `Will create AFT storage directory at ${harness.storageDir.path}`,
    });
  }

  for (const candidate of findOnnxFixCandidates(report)) {
    if (candidate.storageOnnxBytes > 0) {
      items.push({
        kind: "onnx",
        message: `Will replace AFT-managed ONNX cache at ${candidate.storageOnnxDir} (${formatBytes(candidate.storageOnnxBytes)}) and download a compatible runtime`,
      });
    } else {
      items.push({
        kind: "onnx",
        message: `Will leave system ONNX untouched and download a compatible AFT-managed runtime for ${candidate.harness.displayName}`,
      });
    }
  }

  for (const target of findSchemaFixTargets(adapters)) {
    items.push({
      kind: "schema",
      message: `Will add the AFT config $schema URL to ${target.aftConfig} (editor autocomplete + validation)`,
    });
  }

  return items;
}

export function shouldSkipDoctorFixConfirmation(argv: string[]): boolean {
  if (argv.includes("--yes") || argv.includes("-y")) return true;
  if (argv.includes("--ci")) return true;
  return process.stdin.isTTY !== true || process.stdout.isTTY !== true;
}

export function doctorSkewBinaryDownloadDecision(argv: string[]): "prompt" | "proceed" | "skip" {
  if (argv.includes("--yes") || argv.includes("-y")) return "proceed";
  if (argv.includes("--ci")) return "skip";
  if (process.stdin.isTTY !== true || process.stdout.isTTY !== true) return "skip";
  return "prompt";
}

async function confirmDoctorFixPlan(
  plan: readonly DoctorFixPlanItem[],
  argv: string[],
): Promise<boolean> {
  if (plan.length === 0) return true;
  if (shouldSkipDoctorFixConfirmation(argv)) return true;
  return confirm("Apply the planned doctor --fix changes?", false);
}

function logUnmatchedBinaryCandidates(expectedVersion: string): void {
  const probe = probeAftBinary(expectedVersion);
  const unmatched = probe.candidates.filter((candidate) => candidate.status === "unmatched");
  if (unmatched.length === 0) return;

  const expected = probe.expectedMajorMinor
    ? `${probe.expectedMajorMinor}.x`
    : probe.expectedVersion;
  log.warn(`  found unmatched aft binary candidate(s); expected ${expected}:`);
  for (const candidate of unmatched) {
    log.warn(`  unmatched: ${candidate.path} reported v${candidate.version ?? "unknown"}`);
  }
}

/**
 * Detect repairable plugin registration and version config, package cache,
 * storage, schema, native binary, and ONNX issues, then apply them with consent.
 */
async function runFixFlow(
  argv: string[],
  detectOpenCodeHost?: () => OpenCodeHostDetection,
  resolveAdapters: typeof resolveAdaptersForCommand = resolveAdaptersForCommand,
  collect: typeof collectDiagnostics = collectDiagnostics,
): Promise<number> {
  const adapters = await resolveAdapters(argv, {
    allowMulti: false,
    verb: "auto-fix issues for",
  });
  const hostDetection = detectOpenCodeForCommand(adapters, detectOpenCodeHost);
  if (refuseAmbiguousOpenCodeWrites(hostDetection)) {
    outro("Done — no changes made.");
    return 1;
  }
  if (hostDetection?.status === "unknown") {
    log.warn(
      "OpenCode: host generation is unavailable; applying only generation-independent exact-pin config fixes.",
    );
  } else if (hostDetection) {
    log.info(`OpenCode: host generation ${formatHostGenerations(hostDetection)}`);
  }

  log.info("Running diagnostics to identify auto-fixable issues…");
  const report = await collect(adapters);
  if (!report.binaryVersion) {
    logUnmatchedBinaryCandidates(report.cliVersion);
  }

  const plan = buildDoctorFixPlan(adapters, report);
  if (plan.length > 0) {
    log.warn("Planned changes:");
    for (const item of plan) {
      log.info(`  • ${item.message}`);
    }
    if (!(await confirmDoctorFixPlan(plan, argv))) {
      log.info("Skipped — no changes made.");
      outro("Done.");
      return 0;
    }
  }

  await fixPluginEntries(adapters);
  const pluginUpdateSummary = await applyPluginUpdates(findPluginUpdateTargets(adapters, report));
  const storageSummary = ensureStorageDirsForRegisteredPlugins(adapters);

  // Ensure aft.jsonc carries the $schema URL (editor autocomplete + validation).
  // `aft setup` already does this; --fix covers configs created/edited outside
  // setup. Plain `aft doctor` stays read-only and only reports the gap.
  const schemaSummary = applySchemaFixes(findSchemaFixTargets(adapters));

  // GitHub #46 follow-up: download the binary if it's missing. Without this,
  // doctor would silently say "everything looks good" while the user
  // explicitly tried to recover from a wiped cache. ensureBinary first checks
  // the cache (so it's idempotent if the binary was downloaded concurrently
  // by another OpenCode session) and only hits the network when needed.
  let binaryDownloaded = false;
  let binaryDownloadSkipped = false;
  let binaryDownloadError: string | null = null;
  if (!report.binaryVersion) {
    const shouldDownload = await confirmBinaryDownloadDespitePluginSkew(report, argv);
    if (!shouldDownload) {
      binaryDownloadSkipped = true;
    } else {
      log.info("AFT binary not found. Downloading…");
      try {
        // Literal specifier so the bundle inlines aft-bridge instead of
        // resolving the installed package at runtime (whose dist chain loads
        // subc-client's TypeScript entry, which Node cannot load).
        const { ensureBinary } = (await import("@cortexkit/aft-bridge")) as {
          ensureBinary: (version?: string) => Promise<string | null>;
        };
        const path = await ensureBinary(`v${report.cliVersion}`);
        if (path) {
          log.success(`AFT binary installed at ${path}`);
          binaryDownloaded = true;
        } else {
          log.error(
            "AFT binary download failed — no matching release asset on GitHub. " +
              "Try opening any AFT-enabled session to trigger plugin-side download instead.",
          );
          binaryDownloadError = "no matching release asset";
        }
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        log.error(`AFT binary download failed: ${message}`);
        binaryDownloadError = message;
      }
    }
  }

  // Apply the ONNX Runtime repair when diagnostics found a managed-runtime issue.
  const onnxResult = await runOnnxFix(adapters, report, { yes: true });

  // Decide outro state based on combined results. We can have any
  // combination of: ONNX fix attempted/skipped/failed, binary
  // downloaded/skipped/failed, plus pre-existing harness issues left over
  // that this --fix run can't remediate (plugin entry, host install, etc).
  if (
    onnxResult === null &&
    !binaryDownloaded &&
    !binaryDownloadSkipped &&
    !binaryDownloadError &&
    storageSummary.created === 0 &&
    storageSummary.errors === 0 &&
    schemaSummary.changed === 0 &&
    schemaSummary.errors === 0 &&
    pluginUpdateSummary.updated === 0 &&
    pluginUpdateSummary.errors === 0
  ) {
    log.info("No auto-fixable issues detected.");
    note(
      `If you're still seeing 'Semantic Index: failed' in the TUI sidebar, run \`${CLI} doctor\` (without --fix) for a full diagnostic dump.`,
      "Tip",
    );
    const afterReport = await collect(adapters);
    const stillHasProblems = hasDoctorProblems(afterReport);
    outro(stillHasProblems ? "Done — some issues remain." : "Done.");
    return stillHasProblems ? 1 : 0;
  }

  const hadErrors =
    (onnxResult?.errors.length ?? 0) > 0 ||
    binaryDownloadError !== null ||
    storageSummary.errors > 0 ||
    schemaSummary.errors > 0 ||
    pluginUpdateSummary.errors > 0;
  const afterReport = await collectDiagnostics(adapters);
  const stillHasProblems = hasDoctorProblems(afterReport);
  outro(
    hadErrors
      ? "Done — some fixes failed."
      : stillHasProblems
        ? "Done — some issues remain."
        : "Done.",
  );
  return hadErrors || stillHasProblems ? 1 : 0;
}

function logDoctorIssues(report: DiagnosticReport): void {
  const lines = formatDiagnosticIssuesSection(report);
  if (lines.length === 0) return;

  log.warn(lines[0]);
  for (let i = 1; i < lines.length; i += 2) {
    const issue = lines[i];
    const remediation = lines[i + 1];
    if (issue.startsWith("[HIGH]")) {
      log.error(issue);
    } else if (issue.startsWith("[INFO]")) {
      log.info(issue);
    } else {
      log.warn(issue);
    }
    if (remediation) log.warn(remediation);
  }
}

export function formatDoctorStorageStatus(h: DiagnosticReport["harnesses"][number]): string {
  const state = h.storageDir.exists
    ? h.storageDir.path
    : `${h.storageDir.path} (${h.pluginRegistered ? "not yet created (lazy — created on first tool call)" : "not created"})`;
  const legacyDuplication = formatLegacyDuplication(h.storageDir.legacyDuplication);
  return `${state} (${[formatStorageSizes(h.storageDir.sizesByKey), legacyDuplication]
    .filter((part): part is string => Boolean(part))
    .join("; ")})`;
}

async function confirmBinaryDownloadDespitePluginSkew(
  report: DiagnosticReport,
  argv: string[],
): Promise<boolean> {
  const skews = findPluginCliVersionSkews(report);
  if (skews.length === 0) return true;

  log.warn("Plugin/CLI version mismatch detected before binary download:");
  for (const skew of skews) {
    log.warn(`  ${skew.scope}: ${skew.message}`);
    log.warn(`  ${skew.remediation}`);
  }
  log.warn(
    "A newly cached binary will not be used by the older plugin until the plugin is updated.",
  );

  const decision = doctorSkewBinaryDownloadDecision(argv);
  if (decision === "proceed") {
    log.info("Proceeding because --yes/-y was provided.");
    return true;
  }
  if (decision === "skip") {
    log.info(
      `Skipped binary download. Update the plugin to @latest, then rerun \`${CLI} doctor --fix\`.`,
    );
    return false;
  }
  return confirm(
    "Download/cache the CLI-matching binary anyway for after you update the plugin?",
    false,
  );
}

function ensureStorageDirsForRegisteredPlugins(adapters: HarnessAdapter[]): {
  created: number;
  errors: number;
} {
  const summary = { created: 0, errors: 0 };

  for (const adapter of adapters) {
    try {
      if (!adapter.isInstalled() || !adapter.hasPluginEntry()) continue;
      const storageDir = adapter.getStorageDir();
      if (existsSync(storageDir)) continue;
      mkdirSync(storageDir, { recursive: true });
      summary.created += 1;
      log.success(`${adapter.displayName}: created AFT storage directory at ${storageDir}`);
    } catch (err) {
      summary.errors += 1;
      log.error(
        `${adapter.displayName}: failed to create AFT storage directory: ${err instanceof Error ? err.message : String(err)}`,
      );
    }
  }

  return summary;
}

async function clearPluginCache(
  adapter: HarnessAdapter,
  includeBytes: boolean,
): Promise<{ action: "cleared" | "not_applicable" | "not_found" | "error"; bytes: number }> {
  const info = adapter.getPluginCacheInfo();
  const bytes = info.exists ? dirSize(info.path) : 0;
  const result = await adapter.clearPluginCache(true);

  if (result.legacy_path_cleared) {
    log.success(`${adapter.displayName}: legacy_path_cleared at ${result.legacy_path_cleared}`);
  }

  if (result.action === "cleared" || result.action === "legacy_path_cleared") {
    const suffix = includeBytes ? `, reclaimed ${formatBytes(bytes)}` : "";
    log.success(`${adapter.displayName}: cleared plugin cache at ${result.path}${suffix}`);
    return { action: "cleared", bytes };
  }
  if (result.action === "not_applicable") {
    log.info(`${adapter.displayName}: no user-managed plugin cache to clear`);
    return { action: "not_applicable", bytes: 0 };
  }
  if (result.action === "not_found") {
    log.info(`${adapter.displayName}: no plugin cache found at ${result.path}`);
    return { action: "not_found", bytes: 0 };
  }
  if (result.action === "error") {
    log.error(`${adapter.displayName}: cache clear failed: ${result.error ?? "unknown"}`);
    return { action: "error", bytes: 0 };
  }

  return { action: "not_found", bytes: 0 };
}

function reportLspCacheClear(cleanup: ClearResult): void {
  if (cleanup.cleared.length === 0) {
    log.info("LSP install cache: nothing to clear, reclaimed 0 B");
  } else {
    log.success(
      `LSP install cache: cleared ${cleanup.cleared.length} install(s), reclaimed ${formatBytes(cleanup.totalBytes)}`,
    );
  }
  for (const err of cleanup.errors) {
    log.error(`LSP install cache: failed to remove ${err.path}: ${err.error}`);
  }
}

export async function fixPluginEntries(adapters: HarnessAdapter[]): Promise<void> {
  for (const adapter of adapters) {
    await maybeFixPlugin(adapter);
  }
}

async function maybeFixPlugin(adapter: HarnessAdapter): Promise<void> {
  if (!adapter.isInstalled()) return;
  if (!adapter.hasPluginEntry() || adapterConfigNeedsUpdate(adapter)) {
    log.info(`${adapter.displayName}: attempting to register or update plugin config…`);
    const result = await adapter.ensurePluginEntry();
    if (result.ok) {
      log.success(`${adapter.displayName}: ${result.message}`);
    } else {
      log.error(`${adapter.displayName}: ${result.message}`);
    }
  }
  // TUI sidebar entry is setup/doctor-owned, so runtime startup never reverses
  // a user's deliberate removal. Explicit --fix may register or update it.
  if (adapter.ensureTuiPluginEntry && adapter.hasTuiPluginEntry) {
    const hasTuiEntry = adapter.hasTuiPluginEntry();
    if (!hasTuiEntry || adapterConfigNeedsUpdate(adapter, true)) {
      const result = await adapter.ensureTuiPluginEntry();
      if (result.ok && (result.action === "added" || result.action === "updated")) {
        log.success(`${adapter.displayName}: ${result.message}`);
      } else if (!result.ok) {
        log.error(`${adapter.displayName}: ${result.message}`);
      }
    }
  }
}

function describeAdapterInstallHint(kind: string): string {
  if (kind === "opencode") return "https://opencode.ai/docs/install";
  if (kind === "pi") return "https://github.com/badlogic/pi-mono";
  return "(unknown harness)";
}

function formatStorageSizes(sizes: Record<string, number>): string {
  const projectParts = Object.entries(sizes)
    .filter(([key, size]) => key !== "logs" && size > 0)
    .map(([key, size]) => `${key}: ${formatBytes(size)}`);
  const logsSize = sizes.logs ?? 0;
  const parts = [...projectParts];
  if (logsSize > 0) parts.push(`logs: ${formatBytes(logsSize)}`);
  if (projectParts.length > 0) return parts.join(", ");
  return logsSize > 0 ? `${parts[0]}; no project data yet` : "empty";
}

function formatLegacyDuplication(
  summary: DiagnosticReport["harnesses"][number]["storageDir"]["legacyDuplication"],
): string | null {
  if (!summary || summary.totalPartitions === 0) return null;
  const byHarness = summary.byHarness
    .map(
      (entry) => `${entry.harness}: ${entry.partitions} partition(s) / ${formatBytes(entry.bytes)}`,
    )
    .join("; ");
  return `legacy duplication: ${summary.totalPartitions} partition(s), ${formatBytes(summary.totalBytes)} total [${byHarness}]`;
}

interface IssueReviewFile {
  path: string;
  realPath: string;
}

function isInteractiveTerminal(): boolean {
  return process.stdin.isTTY === true && process.stdout.isTTY === true;
}

function issueDescriptionSummaryFromBody(body: string): string {
  const lines = body.split(/\r?\n/);
  const descriptionStart = lines.findIndex((line) => line.trim() === "## Description");
  if (descriptionStart !== -1) {
    const parts: string[] = [];
    for (let i = descriptionStart + 1; i < lines.length; i += 1) {
      const trimmed = lines[i].trim();
      if (trimmed.startsWith("## ")) break;
      if (!trimmed) continue;
      parts.push(trimmed);
      if (parts.join(" ").length >= 72) break;
    }
    const summary = parts.join(" ").replace(/\s+/g, " ").trim();
    if (summary) return summary;
  }

  return (
    lines
      .map((line) => line.trim())
      .find((line) => line.length > 0 && !line.startsWith("#") && !line.startsWith("```")) ??
    "diagnostic report"
  );
}

export function deriveIssueTitleFromBody(body: string): string {
  const summary = issueDescriptionSummaryFromBody(sanitizeContent(body));
  return sanitizeContent(`AFT issue: ${summary.slice(0, 72)}`);
}

function writeIssueReviewFile(body: string): IssueReviewFile | null {
  let reviewDir: string | null = null;
  try {
    reviewDir = mkdtempSync(join(tmpdir(), "aft-issue-"));
    if (process.platform !== "win32") {
      chmodSync(reviewDir, 0o700);
    }
    const outPath = join(reviewDir, "issue.md");
    writeFileSync(outPath, `${body}\n`, { encoding: "utf8", mode: 0o600, flag: "wx" });
    return { path: outPath, realPath: realpathSync(outPath) };
  } catch (err) {
    if (reviewDir) {
      try {
        rmSync(reviewDir, { recursive: true, force: true });
      } catch {
        // ignore cleanup failures after a failed review-file write
      }
    }
    log.error(
      `Failed to write sanitized issue report: ${err instanceof Error ? err.message : String(err)}`,
    );
    return null;
  }
}

function readReviewedIssueFile(reviewFile: IssueReviewFile): string | null {
  try {
    const realPath = realpathSync(reviewFile.path);
    if (realPath !== reviewFile.realPath) {
      log.error(`Review file path changed before filing; refusing to read ${reviewFile.path}.`);
      return null;
    }
    return readFileSync(reviewFile.path, "utf8");
  } catch (err) {
    log.error(
      `Failed to read reviewed issue report: ${err instanceof Error ? err.message : String(err)}`,
    );
    return null;
  }
}

async function promptForIssueSession(adapter: HarnessAdapter): Promise<RecentSession | null> {
  const sessions = listRecentSessions(adapter);
  if (sessions.length === 0) return null;

  const allLogsValue = "__all__";
  const selected = await selectOne("Is this issue about a specific session?", [
    { label: "General — not session-specific (include all logs)", value: allLogsValue },
    ...sessions.map((session) => ({
      label: truncateTitle(session.title),
      value: session.id,
      hint: shortSessionId(session.id),
    })),
  ]);

  if (selected === allLogsValue) return null;
  return sessions.find((session) => session.id === selected) ?? null;
}

function shortSessionId(id: string): string {
  const bareId = id.replace(/^ses_/, "");
  return bareId.length <= 12 ? bareId : bareId.slice(0, 12);
}

/**
 * `aft doctor --issue` flow — collect diagnostics, sanitize user paths,
 * prompt for an issue description, optionally file via `gh`.
 */
async function runIssueFlow(argv: string[]): Promise<number> {
  intro(`${CLI} doctor --issue`);

  if (!isInteractiveTerminal()) {
    note(
      `Non-interactive terminal — not collecting or filing automatically. Run \`${CLI} doctor --issue\` from an interactive terminal so you can describe and review the report before filing.`,
      "Manual filing",
    );
    outro("Done.");
    return 0;
  }

  const adapters = await resolveAdaptersForCommand(argv, {
    allowMulti: false,
    verb: "include in the issue",
  });

  const description = await text("Describe the problem you're running into:", {
    placeholder: "What happened? What did you expect? Steps to reproduce…",
    validate: (value) =>
      value.trim().length === 0 ? "Please enter a short description." : undefined,
  });

  const selectedSession = await promptForIssueSession(adapters[0]);
  const selectedBareSessionId = selectedSession?.id.replace(/^ses_/, "") ?? null;

  const report = await collectDiagnostics(adapters);

  // Build per-harness log sections (last 200 lines each) AND scan a wider
  // window (last 4000 lines per harness, deduped/sanitized) for error-
  // shaped lines that survive even when the main log tail needs heavy
  // truncation to fit GitHub's 64KB body limit.
  const logSections = adapters
    .map((adapter) => {
      const path = adapter.getLogFile();
      const tail = tailLogFile(path, 200);
      const scopedTail = selectedBareSessionId
        ? filterLogToSession(tail, selectedBareSessionId)
        : tail;
      return `#### ${adapter.displayName} log (${path})\n\n\`\`\`\n${scopedTail || "<no log output>"}\n\`\`\`\n`;
    })
    .join("\n");

  // Wider scan (4000 lines per harness) so a flood of recent debug noise
  // doesn't push the actual error out of view. Each harness's wide tail
  // is sanitized independently (sanitizeContent walks the whole string;
  // running it twice on the same content is a no-op), then we extract
  // the 20 most-recent ERROR-shaped lines from the merged result.
  const errorScanWindow = adapters
    .map((adapter) => {
      const path = adapter.getLogFile();
      const tail = tailLogFile(path, 4000);
      const scopedTail = selectedBareSessionId
        ? filterLogToSession(tail, selectedBareSessionId)
        : tail;
      return sanitizeContent(scopedTail);
    })
    .join("\n");
  const recentErrorLines = extractRecentErrors(errorScanWindow, 20);
  const recentErrorsSection =
    recentErrorLines.length === 0
      ? "_No error-shaped log lines found in recent history._"
      : ["```", recentErrorLines.join("\n"), "```"].join("\n");

  const toolFailuresSection = buildRecentAftToolFailuresSectionFromLog();

  const rawBody = [
    "## Description",
    description,
    "",
    "## Environment",
    `- AFT CLI: v${report.cliVersion}`,
    `- AFT binary: ${report.binaryVersion ?? "unknown"}`,
    `- OS: ${report.platform} ${report.arch}`,
    `- Node: ${report.nodeVersion}`,
    ...(selectedSession
      ? [`- Session: ses_${selectedBareSessionId} (${truncateTitle(selectedSession.title)})`]
      : []),
    "",
    "## Diagnostics",
    renderDiagnosticsMarkdown(report),
    "",
    "## Recent errors (last 20, sanitized)",
    recentErrorsSection,
    "",
    toolFailuresSection,
    "",
    "## Logs (last 200 lines per harness)",
    logSections,
    "_Usernames and home paths have been stripped from this report._",
  ].join("\n");

  // Sanitize the entire body (catches any path leakage from sections that
  // weren't already passed through sanitizeContent — diagnostics markdown,
  // description, etc.) and then cap it to GitHub's ~64KB issue-body
  // limit. The cap only shrinks the main `## Logs (last...` block, so
  // the Description/Environment/Diagnostics/Recent errors sections are
  // preserved intact.
  const body = capBodyToGithubLimit(sanitizeContent(rawBody));

  const reviewFile = writeIssueReviewFile(body);
  if (!reviewFile) {
    outro("Done — could not write the issue report.");
    return 1;
  }
  const outPath = reviewFile.path;
  log.success(`Wrote sanitized issue body to ${outPath}`);
  note(
    `Open and review the report before filing:\n  ${outPath}\n\nHome paths and your username have been stripped, but it still contains log lines and file paths from your project. Edit the file to remove anything you don't want public — your edits are used when you confirm below.`,
    "Review before filing",
  );

  // Never file automatically. Only file after the user confirms they have
  // reviewed (and possibly edited) the on-disk report.
  const proceed = await confirm(
    "Have you reviewed the report above? File it as a GitHub issue now?",
    false,
  );
  if (!proceed) {
    note(
      `No issue filed. When ready, file manually at\nhttps://github.com/cortexkit/aft/issues/new and paste the contents of ${outPath}.`,
      "Skipped",
    );
    outro("Done.");
    return 0;
  }

  // Re-read the file so any edits the user made during review are filed, and
  // re-sanitize + re-cap as defense-in-depth in case editing reintroduced a
  // home path or pushed the body over GitHub's limit. The filed title is also
  // derived from this reviewed body so edited-out secrets cannot survive in it.
  const reviewedBody = readReviewedIssueFile(reviewFile);
  if (reviewedBody === null) {
    note(
      "No issue filed. Please review the report path above and file manually if needed.",
      "Skipped",
    );
    outro("Done.");
    return 1;
  }
  const finalBody = capBodyToGithubLimit(sanitizeContent(reviewedBody));
  const finalTitle = deriveIssueTitleFromBody(finalBody);

  if (isGhInstalled()) {
    log.info("Opening GitHub issue via `gh`…");
    const result = createGitHubIssue("cortexkit/aft", finalTitle, finalBody);
    if (result.url) {
      log.success(`Issue filed: ${result.url}`);
      openBrowser(result.url);
      outro("Done.");
      return 0;
    }
    log.warn(`gh failed: ${result.stderr ?? "unknown error"}. Falling back to browser.`);
  }

  const fallback = `https://github.com/cortexkit/aft/issues/new?title=${encodeURIComponent(finalTitle)}&body=${encodeURIComponent(finalBody)}`;
  log.info("Opening GitHub issue form in your browser…");
  openBrowser(fallback);
  note(
    `If the browser didn't open, the sanitized body is at ${outPath}. Copy it into a new issue at https://github.com/cortexkit/aft/issues/new.`,
    "Fallback",
  );
  outro("Done.");
  return 0;
}
