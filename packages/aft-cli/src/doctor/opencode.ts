import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { readJsoncFile } from "../lib/jsonc.js";
import type { OpenCodeHostDetection, OpenCodeHostRuntime } from "../setup/host-generation.js";
import { AFT_OPENCODE_PACKAGE, isAftNpmEntry } from "../setup/opencode-config.js";

export const OPENCODE_LOAD_PATHS = [
  "root-default",
  "export-server-v1",
  "export-server-effect-bun",
  "export-server-effect-node",
] as const;

export type OpenCodeLoadPath = (typeof OPENCODE_LOAD_PATHS)[number];

export interface OpenCodeDoctorInput {
  detection: OpenCodeHostDetection;
  configPath: string;
  logPath: string;
  pluginCachePath: string;
  cachedPluginVersion?: string;
  expectedPluginEntry?: string;
}

export interface OpenCodeDoctorResult {
  expectedLoadPath: OpenCodeLoadPath | null;
  takenLoadPath: OpenCodeLoadPath | null;
  pluginVersion: string | null;
  problems: string[];
}

interface PluginManifest {
  name?: unknown;
  version?: unknown;
  exports?: unknown;
  "oc-plugin"?: unknown;
}

function isLoadPath(value: string): value is OpenCodeLoadPath {
  return (OPENCODE_LOAD_PATHS as readonly string[]).includes(value);
}

function configuredAftEntry(value: Record<string | symbol, unknown> | null): string | null {
  if (!Array.isArray(value?.plugin)) return null;
  for (const entry of value.plugin) {
    if (isAftNpmEntry(entry)) return entry;
    const root = localPluginRoot(entry);
    if (root && manifestFromLocalPath(root)) return entry as string;
  }
  return null;
}

function localPluginRoot(entry: unknown): string | null {
  if (typeof entry !== "string") return null;
  if (entry.startsWith("file://")) {
    try {
      return fileURLToPath(entry);
    } catch {
      return null;
    }
  }
  return entry.startsWith("/") || /^[A-Za-z]:[/\\]/.test(entry) ? entry : null;
}

function configuredVersion(entry: string | null): string | null {
  if (!entry || !isAftNpmEntry(entry)) return null;
  const prefix = `${AFT_OPENCODE_PACKAGE}@`;
  return entry.startsWith(prefix) ? entry.slice(prefix.length) || null : null;
}

function readManifest(path: string): PluginManifest | null {
  try {
    const parsed = JSON.parse(readFileSync(path, "utf8")) as PluginManifest;
    return parsed.name === AFT_OPENCODE_PACKAGE ? parsed : null;
  } catch {
    return null;
  }
}

function manifestFromLocalPath(path: string): PluginManifest | null {
  let current = path;
  if (!existsSync(join(current, "package.json"))) current = dirname(current);
  while (true) {
    const manifest = readManifest(join(current, "package.json"));
    if (manifest) return manifest;
    const parent = dirname(current);
    if (parent === current) return null;
    current = parent;
  }
}

function pluginManifest(entry: string | null, cachePath: string): PluginManifest | null {
  const localRoot = localPluginRoot(entry);
  if (localRoot) return manifestFromLocalPath(localRoot);
  return readManifest(
    join(cachePath, "node_modules", "@cortexkit", "aft-opencode", "package.json"),
  );
}

function manifestUsesServerExport(manifest: PluginManifest | null): boolean {
  if (!manifest) return true;
  const discovered = manifest["oc-plugin"];
  const exports = manifest.exports;
  return (
    Array.isArray(discovered) &&
    discovered.includes("server") &&
    typeof exports === "object" &&
    exports !== null &&
    Object.hasOwn(exports, "./server")
  );
}

function latestLoggedLoadPath(logPath: string): OpenCodeLoadPath | null {
  if (!existsSync(logPath)) return null;
  try {
    const text = readFileSync(logPath, "utf8");
    const pattern =
      /(?:load path|load_path)\s*[:=]\s*(root-default|export-server-v1|export-server-effect-bun|export-server-effect-node)/gi;
    let latest: OpenCodeLoadPath | null = null;
    for (const match of text.matchAll(pattern)) {
      const value = match[1];
      if (value && isLoadPath(value)) latest = value;
    }
    return latest;
  } catch {
    return null;
  }
}

function detectedV2Runtime(detection: OpenCodeHostDetection): OpenCodeHostRuntime {
  return detection.evidence.find((item) => item.generation === "v2")?.runtime ?? "node";
}

export function expectedOpenCodeLoadPath(
  detection: OpenCodeHostDetection,
): OpenCodeLoadPath | null {
  if (detection.status === "v1") return "export-server-v1";
  if (detection.status === "v2") {
    return detectedV2Runtime(detection) === "bun"
      ? "export-server-effect-bun"
      : "export-server-effect-node";
  }
  return null;
}

export function diagnoseOpenCodeLoad(input: OpenCodeDoctorInput): OpenCodeDoctorResult {
  const expectedLoadPath = expectedOpenCodeLoadPath(input.detection);
  const config = readJsoncFile(input.configPath).value;
  const entry = configuredAftEntry(config);
  const manifest = pluginManifest(entry, input.pluginCachePath);
  const logged = latestLoggedLoadPath(input.logPath);
  let takenLoadPath = logged;

  if (!takenLoadPath && input.detection.status === "v1") {
    takenLoadPath = manifestUsesServerExport(manifest) ? "export-server-v1" : "root-default";
  } else if (!takenLoadPath && input.detection.status === "v2") {
    takenLoadPath = expectedLoadPath;
  }

  const pluginVersion =
    (typeof manifest?.version === "string" ? manifest.version : null) ??
    input.cachedPluginVersion ??
    configuredVersion(entry) ??
    null;
  const problems: string[] = [];
  if (config && Object.hasOwn(config, "plugins")) {
    problems.push("config uses unsupported `plugins`; run doctor --fix to migrate it to `plugin`");
  }
  if (
    entry &&
    isAftNpmEntry(entry) &&
    input.expectedPluginEntry &&
    entry !== input.expectedPluginEntry
  ) {
    problems.push(
      `plugin entry ${entry} is not the required exact pin ${input.expectedPluginEntry}`,
    );
  }
  if (input.detection.status === "ambiguous") {
    problems.push("both OpenCode V1 and V2 hosts were detected; refusing configuration writes");
  } else if (input.detection.status === "unknown") {
    problems.push("OpenCode host generation could not be detected");
  }
  if (expectedLoadPath && takenLoadPath && expectedLoadPath !== takenLoadPath) {
    problems.push(
      `load path ${takenLoadPath} does not match ${expectedLoadPath} expected for the detected host`,
    );
  }

  return { expectedLoadPath, takenLoadPath, pluginVersion, problems };
}
