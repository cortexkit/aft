import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { isAbsolute, join } from "node:path";
import {
  mergeIndexes,
  policyPhaseForVersion,
  type RawIndexesConfig,
  resolveCortexKitProjectConfigPath,
  translateConfigDocument,
} from "@cortexkit/aft-bridge";
import { parse as parseJsonc } from "comment-json";

import { CLI } from "./cli.js";

/**
 * What the plugin will do with the AFT config it loads at startup.
 *
 * `doctor` used to read the config only for display, so a config the plugin
 * refuses (and a plugin that therefore registers no tools) still reported
 * "registered, healthy". This mirrors the plugin's startup checks that abort
 * the load: a user/project file that does not parse, retired keys the policy
 * rejects, and a `subc.connection_file` that points at no file. It also
 * derives the effective semantic backend, which decides whether ONNX Runtime
 * is needed at all.
 */

export type PluginLoadBlockerCode =
  | "config_parse_error"
  | "config_rejected"
  | "subc_connection_missing";

export interface PluginLoadBlocker {
  code: PluginLoadBlockerCode;
  /** The config file responsible. */
  path: string;
  message: string;
  /** Exactly what the user (or `doctor --fix`) does about it. */
  remediation: string;
  /** True when `doctor --fix` repairs it (the retired-key migration). */
  fixable: boolean;
}

export interface PluginLoadEvaluation {
  blockers: PluginLoadBlocker[];
  /** Whether the semantic index resolves on. */
  semanticIndex: boolean;
  /** The effective `semantic.backend` (`fastembed` when unset). */
  semanticBackend: string;
  /** True only for the local backend with the semantic index on. */
  onnxRequired: boolean;
}

type Json = Record<string, unknown>;

function isRecord(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function readConfig(path: string): { value: Json | null; error: string | null } {
  if (!existsSync(path)) return { value: null, error: null };
  try {
    const parsed = parseJsonc(readFileSync(path, "utf8"));
    // Drop comment-json's symbol keys so translation sees plain data.
    const plain = JSON.parse(JSON.stringify(parsed)) as unknown;
    return isRecord(plain)
      ? { value: plain, error: null }
      : { value: null, error: "the file is not a JSON object" };
  } catch (error) {
    return { value: null, error: error instanceof Error ? error.message : String(error) };
  }
}

/** Same resolution as the plugin's transport factory: `~` and relative paths are under home. */
export function resolveSubcConnectionPath(raw: string, home: string = homedir()): string {
  const trimmed = raw.trim();
  if (trimmed.startsWith("~")) return join(home, trimmed.slice(1).replace(/^[/\\]/, ""));
  if (isAbsolute(trimmed)) return trimmed;
  return join(home, trimmed);
}

/** A block's own values, then its `harnesses.<harness>` override on top. */
function withHarness(block: Json, harness: string): Json {
  const override = isRecord(block.harnesses) ? block.harnesses[harness] : undefined;
  return isRecord(override) ? { ...block, ...override } : block;
}

function indexesOf(block: Json | null): RawIndexesConfig | undefined {
  return block && isRecord(block.indexes) ? (block.indexes as RawIndexesConfig) : undefined;
}

export interface PluginLoadInput {
  userConfigPath: string;
  /** Project directory whose `.cortexkit/aft.jsonc` the plugin also loads. */
  projectDirectory?: string;
  /** Harness id for `harnesses.<id>` overrides ("opencode" | "pi" | "omp"). */
  harness: string;
  /** Plugin version whose retired-key policy applies. */
  pluginVersion: string;
  home?: string;
}

export function evaluatePluginLoad(input: PluginLoadInput): PluginLoadEvaluation {
  const blockers: PluginLoadBlocker[] = [];
  const phase = policyPhaseForVersion(input.pluginVersion);
  const tiers: { path: string; tier: "user" | "project" }[] = [
    { path: input.userConfigPath, tier: "user" },
  ];
  if (input.projectDirectory) {
    tiers.push({
      path: resolveCortexKitProjectConfigPath(input.projectDirectory),
      tier: "project",
    });
  }

  const loaded: Record<"user" | "project", Json | null> = { user: null, project: null };
  for (const { path, tier } of tiers) {
    const { value, error } = readConfig(path);
    if (error) {
      blockers.push({
        code: "config_parse_error",
        path,
        message: `AFT config ${path} does not parse (${error}); the plugin ignores the whole file and runs on defaults.`,
        remediation: `Fix the JSON/JSONC syntax in ${path}, then restart the host.`,
        fixable: false,
      });
      continue;
    }
    if (!value) continue;
    const translated = structuredClone(value);
    const translation = translateConfigDocument(translated, phase, tier);
    if (translation.errors.length > 0) {
      blockers.push({
        code: "config_rejected",
        path,
        message: `The plugin refuses to start with ${path}: it uses removed keys (${translation.errors
          .map((code) => code.replace(/^removed_config_key:([^:]+):use:(.+)$/, "$1 → $2"))
          .join(", ")}), so no AFT tools are registered.`,
        remediation: `Run \`${CLI} doctor --fix\` to migrate the file, then restart the host.`,
        fixable: true,
      });
    }
    loaded[tier] = withHarness(translated, input.harness);
  }

  // subc selection is user-tier only; a project file can never set it.
  const subc = loaded.user && isRecord(loaded.user.subc) ? loaded.user.subc : null;
  const raw = typeof subc?.connection_file === "string" ? subc.connection_file.trim() : "";
  if (raw.length > 0) {
    const resolved = resolveSubcConnectionPath(raw, input.home);
    if (!existsSync(resolved)) {
      blockers.push({
        code: "subc_connection_missing",
        path: input.userConfigPath,
        message: `The plugin refuses to start: subc.connection_file is set to "${raw}" but ${resolved} does not exist, so no AFT tools are registered.`,
        remediation: `Start the Subconscious daemon that writes ${resolved}, or remove the "subc" block (or its "connection_file" key) from ${input.userConfigPath} to use AFT on its own. doctor --fix does not edit this setting.`,
        fixable: false,
      });
    }
  }

  // Index switches: user base + harness override; a project may only turn an
  // index off. Project files cannot set the semantic backend.
  const indexes = mergeIndexes(indexesOf(loaded.user), indexesOf(loaded.project), true);
  const semanticIndex = indexes?.semantic ?? true;
  const userSemantic = loaded.user && isRecord(loaded.user.semantic) ? loaded.user.semantic : null;
  const semanticBackend =
    typeof userSemantic?.backend === "string" && userSemantic.backend.length > 0
      ? userSemantic.backend
      : "fastembed";
  return {
    blockers,
    semanticIndex,
    semanticBackend,
    onnxRequired: semanticIndex && semanticBackend === "fastembed",
  };
}
