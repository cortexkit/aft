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

import type { HarnessAdapter } from "../adapters/types.js";
import { getBinaryCacheInfo } from "../lib/binary-cache.js";
import { probeAftBinary } from "../lib/binary-probe.js";
import {
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

export async function runDoctor(options: DoctorOptions): Promise<number> {
  if (options.issue) {
    return runIssueFlow(options.argv);
  }
  intro("AFT doctor");

  if (options.fix) {
    return runFixFlow(options.argv);
  }

  if (options.clear) {
    return runClearFlow(options.argv);
  }

  const adapters = await resolveAdaptersForCommand(options.argv, {
    allowMulti: false,
    verb: "diagnose",
  });

  const report = await collectDiagnostics(adapters);

  log.info(`AFT CLI v${report.cliVersion}, AFT binary ${report.binaryVersion ?? "unknown"}`);
  if (!report.binaryVersion) {
    log.warn(
      "  no matching aft binary detected — run `aft doctor --fix` to download, or it will install automatically when an AFT-enabled session makes its first tool call",
    );
    logUnmatchedBinaryCandidates(report.cliVersion);
  }
  log.info(
    `Binary cache: ${report.binaryCache.versions.length} version(s), ${formatBytes(report.binaryCache.totalSize)} at ${report.binaryCache.path}`,
  );

  const npmCount = report.lspCache.npm.entries.length;
  const ghCount = report.lspCache.github.entries.length;
  if (npmCount + ghCount > 0) {
    log.info(
      `LSP cache: ${npmCount} npm + ${ghCount} github install(s), ${formatBytes(report.lspCache.totalSize)} total`,
    );
  }

  const hadProblems = hasDoctorProblems(report);
  for (const h of report.harnesses) {
    log.step(`${h.displayName}`);
    if (!h.hostInstalled) {
      log.warn(`  host not installed — install from: ${describeAdapterInstallHint(h.kind)}`);
      continue;
    }
    log.info(`  host: ${h.hostVersion ?? "unknown version"}`);
    log.info(`  plugin registered: ${h.pluginRegistered ? "yes" : "no"}`);
    log.info(`  plugin version: ${h.pluginCache.cached ?? "not installed"}`);
    if (!h.pluginRegistered) {
      log.warn("  plugin registration can be fixed with `aft setup` or `aft doctor --fix`");
    }

    log.info(`  aft config: ${h.aftConfig.exists ? h.configPaths.aftConfig : "(not set)"}`);
    if (h.aftConfig.parseError) {
      log.error(`  aft config parse error: ${h.aftConfig.parseError}`);
    } else if (h.aftConfig.exists) {
      const { value } = readJsoncFile(h.configPaths.aftConfig);
      const schemaSet = value?.$schema === AFT_SCHEMA_URL;
      log.info(
        `  aft config $schema: ${schemaSet ? "set" : "not set — run `aft doctor --fix` for editor autocomplete"}`,
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
      }
      if (!h.onnxRuntime.cachedPath && !h.onnxRuntime.systemPath) {
        parts.push(`not installed — ${h.onnxRuntime.installHint}`);
      }
      if (h.onnxRuntime.cachedCompatible === false || h.onnxRuntime.systemCompatible === false) {
        parts.push("needs reinstall — run `aft doctor --fix`");
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
    await clearDoctorCaches(adapters, DOCTOR_FORCE_CLEAR_TARGETS, { includePluginBytes: false });
  }

  if (hadProblems) {
    logDoctorIssues(report);
    note(
      "Run `aft setup` or `aft doctor --fix` to register AFT with any harness showing `plugin registered: no`. Run `aft doctor --fix` for ONNX Runtime issues or to download a missing aft binary.",
      "Tips",
    );
    outro("Done — some issues found.");
    return 1;
  }
  outro("Everything looks good.");
  return 0;
}

export function hasDoctorProblems(report: DiagnosticReport): boolean {
  // GitHub #46 follow-up: an absent aft binary is a real problem the user
  // should see flagged, not buried under a misleading "Everything looks
  // good." outro. Reproducer: `rm -rf ~/.cache/aft/bin && bunx --bun
  // @cortexkit/aft doctor` previously printed "AFT binary unknown" + "Binary
  // cache: 0 versions" and then "Everything looks good." at the bottom.
  return formatDiagnosticIssuesSection(report).length > 0;
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
  kind: "plugin" | "binary" | "onnx" | "storage" | "schema";
  message: string;
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
    if (!adapter || harness.pluginRegistered || !harness.hostInstalled) continue;
    if (adapter.kind === "pi") {
      items.push({
        kind: "plugin",
        message: `Will run \`pi install ${adapter.pluginEntryWithVersion}\` to register ${adapter.displayName}`,
      });
    } else {
      items.push({
        kind: "plugin",
        message: `Will add ${adapter.pluginEntryWithVersion} to ${harness.configPaths.harnessConfig}`,
      });
    }
  }

  if (!report.binaryVersion) {
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
    if (!harness.hostInstalled || !harness.pluginRegistered || harness.storageDir.exists) continue;
    items.push({
      kind: "storage",
      message: `Will create AFT storage directory at ${harness.storageDir.path}`,
    });
  }

  for (const candidate of findOnnxFixCandidates(report)) {
    if (candidate.storageOnnxBytes > 0) {
      items.push({
        kind: "onnx",
        message: `Will delete AFT-managed ONNX cache at ${candidate.storageOnnxDir} (${formatBytes(candidate.storageOnnxBytes)})`,
      });
    } else {
      items.push({
        kind: "onnx",
        message: `Will leave system ONNX untouched and refresh AFT-managed ONNX state for ${candidate.harness.displayName} on next start`,
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
 * `aft doctor --fix` flow — detect and apply auto-fixable issues with
 * user consent. Currently covers plugin registration, missing aft binary
 * (GitHub #46 follow-up), and ONNX Runtime version mismatches.
 */
async function runFixFlow(argv: string[]): Promise<number> {
  const adapters = await resolveAdaptersForCommand(argv, {
    allowMulti: false,
    verb: "auto-fix issues for",
  });

  log.info("Running diagnostics to identify auto-fixable issues…");
  const report = await collectDiagnostics(adapters);
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
        const bridgePackageName: string = "@cortexkit/aft-bridge";
        const { ensureBinary } = (await import(bridgePackageName)) as {
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

  // ONNX Runtime fix is the other supported auto-fix today.
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
    schemaSummary.errors === 0
  ) {
    log.info("No auto-fixable issues detected.");
    note(
      "If you're still seeing 'Semantic Index: failed' in the TUI sidebar, run " +
        "`aft doctor` (without --fix) for a full diagnostic dump.",
      "Tip",
    );
    const afterReport = await collectDiagnostics(adapters);
    const stillHasProblems = hasDoctorProblems(afterReport);
    outro(stillHasProblems ? "Done — some issues remain." : "Done.");
    return stillHasProblems ? 1 : 0;
  }

  const hadErrors =
    (onnxResult?.errors.length ?? 0) > 0 ||
    binaryDownloadError !== null ||
    storageSummary.errors > 0 ||
    schemaSummary.errors > 0;
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
  return `${state} (${formatStorageSizes(h.storageDir.sizesByKey)})`;
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
      "Skipped binary download. Update the plugin to @latest, then rerun `aft doctor --fix`.",
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

  if (result.action === "cleared") {
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
  if (!adapter.hasPluginEntry() && adapter.isInstalled()) {
    log.info(`${adapter.displayName}: attempting to register plugin…`);
    const r = await adapter.ensurePluginEntry();
    if (r.ok) {
      log.success(`${adapter.displayName}: ${r.message}`);
    } else {
      log.error(`${adapter.displayName}: ${r.message}`);
    }
  }
}

function describeAdapterInstallHint(kind: string): string {
  if (kind === "opencode") return "https://opencode.ai/docs/install";
  if (kind === "pi") return "https://github.com/badlogic/pi-mono";
  return "(unknown harness)";
}

function formatStorageSizes(sizes: Record<string, number>): string {
  const parts = Object.entries(sizes)
    .filter(([, size]) => size > 0)
    .map(([key, size]) => `${key}: ${formatBytes(size)}`);
  return parts.length > 0 ? parts.join(", ") : "empty";
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
  intro("AFT doctor --issue");

  if (!isInteractiveTerminal()) {
    note(
      "Non-interactive terminal — not collecting or filing automatically. Run `aft doctor --issue` from an interactive terminal so you can describe and review the report before filing.",
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
