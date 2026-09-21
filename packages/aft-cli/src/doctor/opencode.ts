import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { readJsoncFile } from "../lib/jsonc.js";
import type { OpenCodeHostDetection, OpenCodeHostRuntime } from "../setup/host-generation.js";
import {
  AFT_OPENCODE_PACKAGE,
  isAftNpmEntry,
  type OpenCodeConfigGeneration,
  type OpenCodePluginKey,
  openCodePluginKey,
  openCodePluginReadKeys,
  otherOpenCodePluginKey,
  pluginEntryFitsKey,
  pluginEntryPackage,
} from "../setup/opencode-config.js";

export const OPENCODE_LOAD_PATHS = [
  "root-default",
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
  acceptExplicitPluginVersion?: boolean;
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
}

function isLoadPath(value: string): value is OpenCodeLoadPath {
  return (OPENCODE_LOAD_PATHS as readonly string[]).includes(value);
}

/**
 * The AFT registration under one config key, whatever shape it was written in:
 * an npm spec, a local directory or `file://` path, a V1 `[package, options]`
 * tuple, or a V2 `{ package, options }` object.
 */
function aftEntryUnderKey(
  value: Record<string | symbol, unknown> | null,
  key: OpenCodePluginKey,
): string | null {
  const list = value?.[key];
  if (!Array.isArray(list)) return null;
  for (const entry of list) {
    const packageSpec = pluginEntryPackage(entry);
    if (packageSpec === null) continue;
    if (isAftNpmEntry(packageSpec)) return packageSpec;
    const root = localPluginRoot(packageSpec);
    if (root && manifestFromLocalPath(root)) return packageSpec;
  }
  return null;
}

/** The AFT registration the detected host would actually load. */
function configuredAftEntry(
  value: Record<string | symbol, unknown> | null,
  generation: OpenCodeConfigGeneration,
): string | null {
  for (const key of openCodePluginReadKeys(generation)) {
    const entry = aftEntryUnderKey(value, key);
    if (entry) return entry;
  }
  return null;
}

/** Entries under `key` written in the other generation's options shape. */
function misshapenEntries(
  value: Record<string | symbol, unknown> | null,
  key: OpenCodePluginKey,
): string[] {
  const list = value?.[key];
  if (!Array.isArray(list)) return [];
  const packages: string[] = [];
  for (const entry of list) {
    if (pluginEntryFitsKey(entry, key)) continue;
    const packageSpec = pluginEntryPackage(entry);
    if (packageSpec !== null) packages.push(packageSpec);
  }
  return packages;
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

function hasExplicitSemver(entry: string): boolean {
  const version = configuredVersion(entry);
  return Boolean(
    version &&
      /^\d+\.\d+\.\d+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/.test(
        version,
      ),
  );
}

function latestLoggedLoadPath(logPath: string): OpenCodeLoadPath | null {
  if (!existsSync(logPath)) return null;
  try {
    const text = readFileSync(logPath, "utf8");
    const pattern =
      /(?:load path|load_path)\s*[:=]\s*(root-default|export-server-effect-bun|export-server-effect-node)/gi;
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
  if (detection.status === "v1") return "root-default";
  if (detection.status === "v2") {
    return detectedV2Runtime(detection) === "bun"
      ? "export-server-effect-bun"
      : "export-server-effect-node";
  }
  return null;
}

export function diagnoseOpenCodeLoad(input: OpenCodeDoctorInput): OpenCodeDoctorResult {
  const expectedLoadPath = expectedOpenCodeLoadPath(input.detection);
  const generation = input.detection.status;
  const config = readJsoncFile(input.configPath).value;
  const entry = configuredAftEntry(config, generation);
  const manifest = pluginManifest(entry, input.pluginCachePath);
  const logged = latestLoggedLoadPath(input.logPath);
  let takenLoadPath = logged;

  if (!takenLoadPath && generation === "v1") {
    takenLoadPath = "root-default";
  } else if (!takenLoadPath && generation === "v2") {
    takenLoadPath = expectedLoadPath;
  }

  const pluginVersion =
    (typeof manifest?.version === "string" ? manifest.version : null) ??
    input.cachedPluginVersion ??
    configuredVersion(entry) ??
    null;
  const problems: string[] = [...describePluginKeyProblems(config, generation)];
  if (
    entry &&
    isAftNpmEntry(entry) &&
    input.expectedPluginEntry &&
    entry !== input.expectedPluginEntry &&
    !(input.acceptExplicitPluginVersion && hasExplicitSemver(entry))
  ) {
    problems.push(
      `plugin entry ${entry} is not the required exact pin ${input.expectedPluginEntry}`,
    );
  }
  if (generation === "ambiguous") {
    problems.push(
      "both OpenCode V1 and V2 hosts were detected; refusing configuration writes (a V1 host reads `plugin`, a V2 host reads `plugins`)",
    );
  } else if (generation === "unknown") {
    problems.push("OpenCode host generation could not be detected");
  }
  if (expectedLoadPath && takenLoadPath && expectedLoadPath !== takenLoadPath) {
    problems.push(
      `load path ${takenLoadPath} does not match ${expectedLoadPath} expected for the detected host`,
    );
  }

  return { expectedLoadPath, takenLoadPath, pluginVersion, problems };
}

/**
 * Report registrations the detected host cannot load: AFT under the other
 * generation's key, and entries written in the other generation's options
 * shape. Entries AFT did not write are reported and left alone — rewriting a
 * user's other plugins is not doctor's call.
 */
function describePluginKeyProblems(
  config: Record<string | symbol, unknown> | null,
  generation: OpenCodeConfigGeneration,
): string[] {
  if (!config || generation === "ambiguous" || generation === "unknown") return [];
  const hostKey = openCodePluginKey(generation);
  const otherKey = otherOpenCodePluginKey(hostKey);
  const host = generation.toUpperCase();
  const problems: string[] = [];

  if (!aftEntryUnderKey(config, hostKey) && aftEntryUnderKey(config, otherKey)) {
    problems.push(
      `AFT is registered under \`${otherKey}\`, which a ${host} host does not read; run doctor --fix to register it under \`${hostKey}\``,
    );
  }

  const aftEntry = aftEntryUnderKey(config, hostKey);
  for (const packageSpec of misshapenEntries(config, hostKey)) {
    problems.push(
      packageSpec === aftEntry
        ? `plugin entry ${packageSpec} under \`${hostKey}\` uses the ${host === "V1" ? "V2" : "V1"} entry shape; run doctor --fix to rewrite it`
        : `plugin entry ${packageSpec} under \`${hostKey}\` uses the ${host === "V1" ? "V2" : "V1"} entry shape, which a ${host} host cannot load; left unchanged because AFT did not register it`,
    );
  }
  return problems;
}
