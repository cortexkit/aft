import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import { log, sessionLog } from "./logger.js";

const WARNING_MARKER = "🔧 AFT: ⚠️";
const FEATURE_MARKER = "🔧 AFT: ✨";

export interface ConfigureWarning {
  kind: "formatter_not_installed" | "checker_not_installed" | "lsp_binary_missing";
  language?: string;
  server?: string;
  tool?: string;
  binary?: string;
  hint: string;
}

export interface ConfigureWarningOptions {
  client: unknown;
  sessionId: string;
  bridge: Pick<BinaryBridge, "send">;
  storageDir: string;
  pluginVersion: string;
  projectRoot?: string;
}

type PiNotificationClient = {
  ui?: {
    notify?: (message: string, type?: "info" | "warning" | "error") => void;
  };
};

function sendIgnoredMessage(client: unknown, sessionId: string, text: string): boolean {
  const typedClient = client as PiNotificationClient;
  if (typeof typedClient.ui?.notify !== "function") return false;

  try {
    typedClient.ui.notify(text, "warning");
    return true;
  } catch (err) {
    sessionLog(
      sessionId,
      `[aft-pi] notification send failed: ${err instanceof Error ? err.message : String(err)}`,
    );
    return false;
  }
}

async function readWarnedTools(
  bridge: Pick<BinaryBridge, "send">,
): Promise<Record<string, unknown>> {
  try {
    const resp = await bridge.send("db_get_state", { key: "warned_tools" });
    if (resp.success === false) return {};

    const value = (resp.data as { value?: unknown } | undefined)?.value;
    if (typeof value !== "string") return {};

    const parsed = JSON.parse(value) as unknown;
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
    return parsed as Record<string, unknown>;
  } catch {
    return {};
  }
}

async function hasWarnedFor(bridge: Pick<BinaryBridge, "send">, key: string): Promise<boolean> {
  const warned = await readWarnedTools(bridge);
  return warned[key] === true || typeof warned[key] === "string";
}

async function recordWarning(bridge: Pick<BinaryBridge, "send">, key: string): Promise<void> {
  const warned = await readWarnedTools(bridge);
  warned[key] = true;

  try {
    await bridge.send("db_set_state", {
      key: "warned_tools",
      value: JSON.stringify(warned),
    });
  } catch {
    // best-effort
  }
}

function warningKey(warning: ConfigureWarning, projectRoot?: string): string {
  const scope = warning.kind === "lsp_binary_missing" ? "_" : (projectRoot ?? "_");
  return [
    scope,
    warning.kind,
    warning.language ?? warning.server ?? "_",
    warning.tool ?? warning.binary ?? "_",
    warning.hint,
  ]
    .map((part) => encodeURIComponent(part))
    .join(":");
}

function warningTitle(warning: ConfigureWarning): string {
  switch (warning.kind) {
    case "formatter_not_installed":
      return "Formatter is not installed";
    case "checker_not_installed":
      return "Checker is not installed";
    case "lsp_binary_missing":
      return "LSP binary is missing";
  }
}

function formatConfigureWarning(warning: ConfigureWarning): string {
  const details: string[] = [];
  if (warning.language) details.push(`language: ${warning.language}`);
  if (warning.server) details.push(`server: ${warning.server}`);
  if (warning.tool) details.push(`tool: ${warning.tool}`);
  if (warning.binary && warning.binary !== warning.tool) {
    details.push(`binary: ${warning.binary}`);
  }

  const suffix = details.length > 0 ? ` (${details.join(", ")})` : "";
  return `${WARNING_MARKER} ${warningTitle(warning)}${suffix}\n${warning.hint}`;
}

export async function deliverConfigureWarnings(
  opts: ConfigureWarningOptions,
  warnings: ConfigureWarning[],
): Promise<void> {
  if (warnings.length === 0) return;

  // `warned_tools` now persists through the bridge DB state API. This loses the
  // old file-lock read-modify-write mutex, so two same-process concurrent
  // recordWarning calls could race and drop one key. Configure warnings are
  // delivered sequentially in normal plugin flow; if this becomes observable,
  // add a bridge-side atomic update command rather than reviving file locks.
  for (const warning of warnings) {
    const key = warningKey(warning, opts.projectRoot);
    if (await hasWarnedFor(opts.bridge, key)) continue;

    if (!sendIgnoredMessage(opts.client, opts.sessionId, formatConfigureWarning(warning))) continue;

    await recordWarning(opts.bridge, key);
  }
}

export function sendFeatureAnnouncement(
  version: string,
  features: string[],
  footer: string,
  storageDir: string,
): void {
  // v0.27 commit 11 deferral: the legacy `last_announced_version` file is read at
  // plugin init, BEFORE any bridge is spawned (lazy-spawn architecture per commit
  // 29508a5). Refactoring to `bridge.send("db_get_state")` would force eager bridge
  // spawn at every plugin init. Deferred to a future version that decides whether
  // to accept that trade-off. The Rust-side dual-write from commit 10 covers any
  // other writer; this file stays in sync via direct legacy-file writes.
  const versionFile = join(storageDir, "last_announced_version");
  try {
    if (existsSync(versionFile)) {
      const lastVersion = readFileSync(versionFile, "utf-8").trim();
      if (lastVersion === version) return;
    }
  } catch {
    // ignore read errors — proceed with announcement
  }

  // Blank-line separator pins the persistent footer (Discord invite, etc.)
  // below the version-specific bullets so the footer reads as "always here"
  // rather than as one more changelog item.
  const sections: string[] = [
    `${FEATURE_MARKER} v${version}:`,
    ...features.map((feature) => `  • ${feature}`),
  ];
  if (typeof footer === "string" && footer.trim().length > 0) {
    sections.push("", footer);
  }
  log(sections.join("\n"));

  try {
    mkdirSync(storageDir, { recursive: true });
    writeFileSync(versionFile, version);
  } catch {
    // best-effort
  }
}
