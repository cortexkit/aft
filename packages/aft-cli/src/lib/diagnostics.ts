import {
  accessSync,
  closeSync,
  constants,
  existsSync,
  openSync,
  readSync,
  statSync,
} from "node:fs";
import { resolveCortexKitProjectConfigPath } from "@cortexkit/aft-bridge";

import type { HarnessAdapter } from "../adapters/types.js";
import { type BinaryCacheInfo, getBinaryCacheInfo } from "./binary-cache.js";
import { probeBinaryVersion } from "./binary-probe.js";
import { type BuildBreakerSuspension, readBuildBreakerSuspensions } from "./build-breaker.js";
import { CLI } from "./cli.js";
import { readJsoncFile } from "./jsonc.js";
import type { LegacyPartitionDuplicationSummary } from "./legacy-storage.js";
import { summarizeLegacyPartitionDuplication } from "./legacy-storage.js";
import { getLspCacheReport, type LspCacheReport } from "./lsp-cache.js";
import {
  detectOrtVersion,
  findCachedOnnxRuntime,
  findIgnoredWindowsSystemOnnxRuntime,
  findSystemOnnxRuntime,
  getManualInstallHint,
  isOrtVersionCompatible,
  REQUIRED_ORT_MAJOR,
  REQUIRED_ORT_MIN_MINOR,
} from "./onnx.js";
import { sanitizeValue } from "./sanitize.js";
import { getSelfVersion } from "./self-version.js";

export interface DiagnosticReport {
  timestamp: string;
  platform: string;
  arch: string;
  nodeVersion: string;
  cliVersion: string;
  binaryVersion: string | null;
  harnesses: HarnessDiagnostic[];
  binaryCache: BinaryCacheInfo;
  /** LSP package and binary caches populated by plugin auto-install. */
  lspCache: LspCacheReport;
  /** Active durable build-breaker suspensions, grouped by their stored project root. */
  buildBreakerSuspensions?: BuildBreakerSuspension[];
}

export type DiagnosticIssueSeverity = "high" | "medium" | "low" | "info";

export interface DiagnosticIssue {
  code:
    | "binary_missing"
    | "host_missing"
    | "plugin_missing"
    | "config_parse_error"
    | "plugin_cli_version_skew"
    | "onnx_missing"
    | "onnx_incompatible";
  severity: DiagnosticIssueSeverity;
  scope: string;
  message: string;
  remediation: string;
}

export interface HarnessDiagnostic {
  kind: string;
  displayName: string;
  hostInstalled: boolean;
  hostVersion: string | null;
  pluginRegistered: boolean;
  configPaths: ReturnType<HarnessAdapter["detectConfigPaths"]>;
  aftConfig: {
    exists: boolean;
    parseError?: string;
    enabled: boolean;
    enabledSource?: string;
    flags: Record<string, unknown>;
  };
  pluginCache: ReturnType<HarnessAdapter["getPluginCacheInfo"]>;
  storageDir: {
    path: string;
    /** True when the storage directory is present on disk. */
    exists: boolean;
    /** True when the directory exists and is readable + writable. */
    accessible: boolean;
    sizesByKey: Record<string, number>;
    legacyDuplication?: LegacyPartitionDuplicationSummary;
  };
  onnxRuntime: {
    required: boolean;
    systemPath: string | null;
    systemVersion: string | null;
    systemCompatible: boolean | null;
    ignoredSystemPath?: string | null;
    ignoredSystemReason?: string | null;
    cachedPath: string | null;
    cachedVersion: string | null;
    cachedCompatible: boolean | null;
    platform: string;
    installHint: string;
    requirement: string;
  };
  logFile: {
    path: string;
    exists: boolean;
    sizeKb: number;
  };
}

export async function collectDiagnostics(adapters: HarnessAdapter[]): Promise<DiagnosticReport> {
  const cliVersion = getSelfVersion();
  const binaryVersion = probeBinaryVersion(cliVersion);

  const harnesses: HarnessDiagnostic[] = [];
  for (const adapter of adapters) {
    harnesses.push(await diagnoseHarness(adapter));
  }

  return {
    timestamp: new Date().toISOString(),
    platform: process.platform,
    arch: process.arch,
    nodeVersion: process.version,
    cliVersion,
    binaryVersion,
    harnesses,
    binaryCache: getBinaryCacheInfo(cliVersion),
    lspCache: getLspCacheReport(),
    buildBreakerSuspensions: harnesses[0]
      ? readBuildBreakerSuspensions(harnesses[0].storageDir.path)
      : [],
  };
}

async function diagnoseHarness(adapter: HarnessAdapter): Promise<HarnessDiagnostic> {
  const configPaths = adapter.detectConfigPaths();
  const aftConfigRead = readJsoncFile(configPaths.aftConfig);
  const aftFlags = (sanitizeValue(aftConfigRead.value ?? {}) as Record<string, unknown>) ?? {};
  const projectConfigPath = resolveCortexKitProjectConfigPath(process.cwd());
  const projectConfigRead = readJsoncFile(projectConfigPath);
  const userEnabled = typeof aftFlags.enabled === "boolean" ? aftFlags.enabled : undefined;
  const projectEnabled =
    typeof projectConfigRead.value?.enabled === "boolean"
      ? projectConfigRead.value.enabled
      : undefined;
  const aftEnabled = projectEnabled ?? userEnabled ?? true;
  const aftEnabledSource =
    projectEnabled !== undefined
      ? projectConfigPath
      : userEnabled !== undefined
        ? configPaths.aftConfig
        : undefined;
  const storage = adapter.getStorageDir();
  const logPath = adapter.getLogFile();
  const pluginCache = adapter.getPluginCacheInfo();

  // Check if the storage directory exists and is accessible. We do NOT create
  // it here — the doctor command is a diagnostic tool and should not mutate
  // filesystem state. The bridge creates it lazily on first tool call.
  const storageAccessible = (() => {
    if (!existsSync(storage)) return false;
    try {
      accessSync(storage, constants.R_OK | constants.W_OK);
      return true;
    } catch {
      return false;
    }
  })();

  const describeStorage =
    "describeStorageSubtrees" in adapter &&
    typeof (adapter as unknown as { describeStorageSubtrees: () => Record<string, number> })
      .describeStorageSubtrees === "function"
      ? (
          adapter as unknown as { describeStorageSubtrees: () => Record<string, number> }
        ).describeStorageSubtrees()
      : {};
  const legacyDuplication = summarizeLegacyPartitionDuplication(storage);

  const semanticEnabled =
    aftEnabled &&
    ((aftConfigRead.value as Record<string, unknown> | null)?.semantic_search === true ||
      (aftConfigRead.value as Record<string, unknown> | null)?.experimental_semantic_search ===
        true);

  const systemOrtDir = findSystemOnnxRuntime();
  const ignoredSystemOrtDir = systemOrtDir ? null : findIgnoredWindowsSystemOnnxRuntime();
  const cachedOrtDir = findCachedOnnxRuntime(storage);
  const systemVersion = systemOrtDir ? detectOrtVersion(systemOrtDir) : null;
  const cachedVersion = cachedOrtDir ? detectOrtVersion(cachedOrtDir) : null;

  return {
    kind: adapter.kind,
    displayName: adapter.displayName,
    hostInstalled: adapter.isInstalled(),
    hostVersion: adapter.getHostVersion(),
    pluginRegistered: adapter.hasPluginEntry(),
    configPaths,
    aftConfig: {
      exists: existsSync(configPaths.aftConfig),
      ...(aftConfigRead.error ? { parseError: aftConfigRead.error } : {}),
      enabled: aftEnabled,
      ...(aftEnabledSource ? { enabledSource: aftEnabledSource } : {}),
      flags: aftFlags,
    },
    pluginCache,
    storageDir: {
      path: storage,
      exists: existsSync(storage),
      accessible: storageAccessible,
      sizesByKey: describeStorage,
      ...(legacyDuplication.totalPartitions > 0 ? { legacyDuplication } : {}),
    },
    onnxRuntime: {
      required: semanticEnabled,
      systemPath: systemOrtDir,
      systemVersion,
      systemCompatible: systemVersion ? isOrtVersionCompatible(systemVersion) : null,
      ignoredSystemPath: ignoredSystemOrtDir,
      ignoredSystemReason: ignoredSystemOrtDir
        ? "version unreadable (Windows system copy) — ignored"
        : null,
      cachedPath: cachedOrtDir,
      cachedVersion,
      cachedCompatible: cachedVersion ? isOrtVersionCompatible(cachedVersion) : null,
      platform: `${process.platform}-${process.arch}`,
      installHint: getManualInstallHint(),
      requirement: `>=${REQUIRED_ORT_MAJOR}.${REQUIRED_ORT_MIN_MINOR}`,
    },
    logFile: {
      path: logPath,
      exists: existsSync(logPath),
      sizeKb: existsSync(logPath) ? Math.round(statSync(logPath).size / 1024) : 0,
    },
  };
}

export function renderDiagnosticsMarkdown(report: DiagnosticReport): string {
  const lines: string[] = [];
  lines.push(`- Timestamp: ${report.timestamp}`);
  // Use explicit `AFT CLI` / `AFT binary` labels so users with multiple
  // harnesses (Pi v0.74.0, OpenCode v0.x.y) can tell at a glance that
  // these are AFT's own versions, not the host's.
  lines.push(`- AFT CLI: v${report.cliVersion}`);
  lines.push(`- AFT binary: ${report.binaryVersion ?? "unknown"}`);
  lines.push(`- OS: ${report.platform} ${report.arch}`);
  lines.push(`- Node: ${report.nodeVersion}`);

  const issues = collectDiagnosticIssues(report);
  if (issues.length > 0) {
    lines.push("");
    lines.push("### Issues found");
    for (const issue of issues) {
      lines.push(
        `- **${issue.severity.toUpperCase()}** ${issue.scope}: ${issue.message} Remediation: ${issue.remediation}`,
      );
    }
  }

  for (const h of report.harnesses) {
    lines.push("");
    lines.push(`### ${h.displayName}`);
    // Always render host version on its own line so its absence is explicit
    // ("unknown" rather than silently omitted). Triage tip: if hostInstalled
    // is true but hostVersion is unknown, the host's `--version` flag failed
    // — file an issue with the harness logs, not aft logs.
    lines.push(`- Host installed: ${h.hostInstalled}`);
    lines.push(`- Host version: ${h.hostVersion ?? "unknown"}`);
    lines.push(`- Plugin registered: ${h.pluginRegistered}`);
    lines.push(`- Plugin version: ${h.pluginCache.cached ?? "not installed"}`);
    lines.push(
      `- AFT enabled: ${h.aftConfig.enabled}${h.aftConfig.enabledSource ? ` (from ${h.aftConfig.enabledSource})` : ""}`,
    );
    lines.push(`- AFT config parse error: ${h.aftConfig.parseError ?? "none"}`);
    lines.push("");
    lines.push("#### Config paths");
    lines.push("```json");
    lines.push(JSON.stringify(h.configPaths, null, 2));
    lines.push("```");
    lines.push("");
    lines.push("#### AFT flags");
    lines.push("```json");
    lines.push(JSON.stringify(h.aftConfig.flags, null, 2));
    lines.push("```");
    lines.push("");
    lines.push("#### Plugin cache");
    lines.push("```json");
    lines.push(JSON.stringify(h.pluginCache, null, 2));
    lines.push("```");
    lines.push("");
    lines.push("#### Storage");
    lines.push("```json");
    lines.push(JSON.stringify(h.storageDir, null, 2));
    lines.push("```");
    lines.push("");
    lines.push("#### ONNX Runtime");
    lines.push("```json");
    lines.push(JSON.stringify(h.onnxRuntime, null, 2));
    lines.push("```");
    lines.push("");
    lines.push(`#### Log file`);
    lines.push(
      `\`${h.logFile.path}\` (${h.logFile.exists ? `${h.logFile.sizeKb} KB` : "missing"})`,
    );
  }

  lines.push("");
  lines.push("### Binary cache");
  lines.push("```json");
  lines.push(JSON.stringify(report.binaryCache, null, 2));
  lines.push("```");

  lines.push("");
  lines.push("### LSP cache");
  lines.push("```json");
  lines.push(JSON.stringify(report.lspCache, null, 2));
  lines.push("```");
  return lines.join("\n");
}

function normalizeVersion(version: string): string {
  return version.trim().replace(/^v/, "");
}

function compareLooseSemver(a: string, b: string): number {
  const aParts = normalizeVersion(a)
    .split(/[.-]/)
    .slice(0, 3)
    .map((part) => Number.parseInt(part, 10));
  const bParts = normalizeVersion(b)
    .split(/[.-]/)
    .slice(0, 3)
    .map((part) => Number.parseInt(part, 10));
  for (let i = 0; i < 3; i += 1) {
    const av = Number.isFinite(aParts[i]) ? aParts[i] : 0;
    const bv = Number.isFinite(bParts[i]) ? bParts[i] : 0;
    if (av !== bv) return av - bv;
  }
  return 0;
}

function pluginPackageNameForHarness(kind: string): string {
  if (kind === "pi" || kind === "omp") return "@cortexkit/aft-pi";
  return "@cortexkit/aft-opencode";
}

function pluginVersionSkewIssue(
  harness: HarnessDiagnostic,
  cliVersion: string,
): DiagnosticIssue | null {
  const pluginVersion = harness.pluginCache.cached;
  if (!pluginVersion) return null;
  if (normalizeVersion(pluginVersion) === normalizeVersion(cliVersion)) return null;

  const relation =
    compareLooseSemver(pluginVersion, cliVersion) < 0 ? "older than" : "different from";
  const packageName = pluginPackageNameForHarness(harness.kind);
  return {
    code: "plugin_cli_version_skew",
    severity: "high",
    scope: harness.displayName,
    message:
      relation === "older than"
        ? `Plugin version (${pluginVersion}) is older than CLI (${cliVersion}). New binary cache won't be used until you update the plugin.`
        : `Plugin version (${pluginVersion}) does not match CLI (${cliVersion}). New binary cache may not be used until the plugin and CLI match.`,
    remediation: `Update \`${packageName}\` in your harness config to \`@latest\`.`,
  };
}

export function collectDiagnosticIssues(report: DiagnosticReport): DiagnosticIssue[] {
  const issues: DiagnosticIssue[] = [];

  const hasEnabledHarness = report.harnesses.some((h) => h.hostInstalled && h.aftConfig.enabled);
  const hasMatchingEnabledPlugin = report.harnesses.some(
    (h) =>
      h.pluginRegistered &&
      h.aftConfig.enabled &&
      h.pluginCache.cached &&
      normalizeVersion(h.pluginCache.cached) === normalizeVersion(report.cliVersion),
  );
  if (!report.binaryVersion && hasEnabledHarness) {
    issues.push({
      code: "binary_missing",
      severity: hasMatchingEnabledPlugin ? "info" : "high",
      scope: "AFT binary",
      message: hasMatchingEnabledPlugin
        ? `No aft binary matching CLI ${report.cliVersion} was detected; it will self-install when the next AFT-enabled session starts.`
        : `No aft binary matching CLI ${report.cliVersion} was detected.`,
      remediation: hasMatchingEnabledPlugin
        ? `Start an AFT-enabled session to install the matching binary automatically, or run \`${CLI} doctor --fix\`.`
        : `Run \`${CLI} doctor --fix\` to download the matching binary, or start an AFT-enabled session to trigger plugin-side install.`,
    });
  }

  for (const h of report.harnesses) {
    if (!h.hostInstalled) {
      issues.push({
        code: "host_missing",
        severity: "high",
        scope: h.displayName,
        message: "Host CLI is not installed or is not on PATH.",
        remediation: `Install ${h.displayName} and rerun \`${CLI} doctor\`.`,
      });
      continue;
    }

    if (!h.pluginRegistered) {
      issues.push({
        code: "plugin_missing",
        severity: "medium",
        scope: h.displayName,
        message: "AFT plugin is not registered with this harness.",
        remediation: `Run \`${CLI} setup\` or \`${CLI} doctor --fix\` to register the plugin.`,
      });
    }

    if (h.aftConfig.parseError) {
      issues.push({
        code: "config_parse_error",
        severity: "high",
        scope: h.displayName,
        message: `AFT config parse error: ${h.aftConfig.parseError}`,
        remediation: `Fix JSON/JSONC syntax in ${h.configPaths.aftConfig}.`,
      });
    }

    const skewIssue = pluginVersionSkewIssue(h, report.cliVersion);
    if (h.aftConfig.enabled && skewIssue) issues.push(skewIssue);

    if (h.aftConfig.enabled && h.onnxRuntime.required) {
      if (!h.onnxRuntime.cachedPath && !h.onnxRuntime.systemPath) {
        issues.push({
          code: "onnx_missing",
          severity: "medium",
          scope: h.displayName,
          message: h.onnxRuntime.ignoredSystemPath
            ? `ONNX Runtime at ${h.onnxRuntime.ignoredSystemPath} has a ${h.onnxRuntime.ignoredSystemReason}; no compatible runtime was detected.`
            : "ONNX Runtime is required for semantic search but was not detected.",
          remediation: `Run \`${CLI} doctor --fix\` or install ONNX Runtime manually (${h.onnxRuntime.installHint}).`,
        });
      }
      if (h.onnxRuntime.cachedCompatible === false) {
        issues.push({
          code: "onnx_incompatible",
          severity: "medium",
          scope: h.displayName,
          message: `Cached ONNX Runtime ${h.onnxRuntime.cachedVersion ?? "unknown"} is incompatible (requires ${h.onnxRuntime.requirement}).`,
          remediation: `Run \`${CLI} doctor --fix\` to refresh AFT-managed ONNX Runtime state.`,
        });
      }
      if (h.onnxRuntime.systemCompatible === false) {
        issues.push({
          code: "onnx_incompatible",
          severity: "medium",
          scope: h.displayName,
          message: `System ONNX Runtime ${h.onnxRuntime.systemVersion ?? "unknown"} is incompatible (requires ${h.onnxRuntime.requirement}).`,
          remediation: "Install a compatible ONNX Runtime or let AFT use its managed runtime.",
        });
      }
    }
  }

  return issues;
}

export function findPluginCliVersionSkews(report: DiagnosticReport): DiagnosticIssue[] {
  return collectDiagnosticIssues(report).filter(
    (issue) => issue.code === "plugin_cli_version_skew",
  );
}

export function formatDiagnosticIssuesSection(report: DiagnosticReport): string[] {
  const issues = collectDiagnosticIssues(report);
  if (issues.length === 0) return [];

  const lines = ["--- Issues found ---"];
  for (const issue of issues) {
    lines.push(`[${issue.severity.toUpperCase()}] ${issue.scope}: ${issue.message}`);
    lines.push(`  Remediation: ${issue.remediation}`);
  }
  return lines;
}

/** Utility: read the tail of a log file, best-effort. */
export function tailLogFile(path: string, lines: number): string {
  if (!existsSync(path)) return "";
  if (lines <= 0) return "";
  const chunkSize = 64 * 1024;
  let fd: number | null = null;
  try {
    const size = statSync(path).size;
    fd = openSync(path, "r");
    const chunks: Buffer[] = [];
    let position = size;
    let newlineCount = 0;

    while (position > 0 && newlineCount <= lines) {
      const readLength = Math.min(chunkSize, position);
      position -= readLength;
      const buffer = Buffer.allocUnsafe(readLength);
      const bytesRead = readSync(fd, buffer, 0, readLength, position);
      const chunk = bytesRead === readLength ? buffer : buffer.subarray(0, bytesRead);
      chunks.unshift(chunk);
      for (let i = chunk.length - 1; i >= 0; i -= 1) {
        if (chunk[i] === 10) newlineCount += 1;
      }
    }

    return Buffer.concat(chunks)
      .toString("utf-8")
      .trimEnd()
      .split(/\r?\n/)
      .slice(-lines)
      .join("\n")
      .trim();
  } catch {
    return "";
  } finally {
    if (fd !== null) {
      try {
        closeSync(fd);
      } catch {
        // ignore close errors in best-effort diagnostics
      }
    }
  }
}
