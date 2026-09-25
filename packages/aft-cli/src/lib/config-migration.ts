import { existsSync, readFileSync } from "node:fs";
import { resolveCortexKitProjectConfigPath, translateConfigDocument } from "@cortexkit/aft-bridge";
import { parse as parseJsonc } from "comment-json";

/**
 * Preview and report the retired-key config migration that `doctor --fix`
 * runs through the native `aft fix-config`.
 *
 * The preview applies the shared TypeScript translation (kept equal to the
 * native one by the config parity fixtures) to a copy of each file, so the
 * plan can say what will change before anything is written. The report after
 * the run diffs each file as it was before against what the binary wrote, so
 * the user sees exactly what happened to their config, not a guess.
 */

export type ConfigTier = "user" | "project";

export interface ConfigMigrationTarget {
  path: string;
  tier: ConfigTier;
}

export interface ConfigChange {
  kind: "removed" | "added" | "changed";
  key: string;
  before?: unknown;
  after?: unknown;
}

export interface ConfigMigrationPreview extends ConfigMigrationTarget {
  changes: ConfigChange[];
}

type Json = Record<string, unknown>;

function isRecord(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Parse a config file into plain data, or null when it is missing or unreadable. */
export function readPlainConfig(path: string): Json | null {
  if (!existsSync(path)) return null;
  try {
    const plain = JSON.parse(JSON.stringify(parseJsonc(readFileSync(path, "utf8")))) as unknown;
    return isRecord(plain) ? plain : null;
  } catch {
    return null;
  }
}

/** The files the migration touches: the user file and the project file at `cwd`, when present. */
export function configMigrationTargets(
  userConfigPath: string,
  cwd: string = process.cwd(),
): ConfigMigrationTarget[] {
  const targets: ConfigMigrationTarget[] = [];
  if (existsSync(userConfigPath)) targets.push({ path: userConfigPath, tier: "user" });
  const project = resolveCortexKitProjectConfigPath(cwd);
  if (project !== userConfigPath && existsSync(project)) {
    targets.push({ path: project, tier: "project" });
  }
  return targets;
}

/** Leaf-level differences between two config objects; arrays compare as whole values. */
export function diffConfig(before: Json, after: Json, prefix = ""): ConfigChange[] {
  const changes: ConfigChange[] = [];
  const keys = new Set([...Object.keys(before), ...Object.keys(after)]);
  for (const key of keys) {
    if (key === "$schema") continue;
    const path = prefix ? `${prefix}.${key}` : key;
    const had = Object.hasOwn(before, key);
    const has = Object.hasOwn(after, key);
    const left = before[key];
    const right = after[key];
    if (had && has && isRecord(left) && isRecord(right)) {
      changes.push(...diffConfig(left, right, path));
    } else if (had && !has) {
      // A removed object is listed leaf by leaf, like an edited one.
      if (isRecord(left) && Object.keys(left).length > 0)
        changes.push(...diffConfig(left, {}, path));
      else changes.push({ kind: "removed", key: path, before: left });
    } else if (!had && has) {
      if (isRecord(right) && Object.keys(right).length > 0)
        changes.push(...diffConfig({}, right, path));
      else changes.push({ kind: "added", key: path, after: right });
    } else if (JSON.stringify(left) !== JSON.stringify(right)) {
      changes.push({ kind: "changed", key: path, before: left, after: right });
    }
  }
  return changes;
}

/** Files that still use retired keys, with the changes the migration will make to each. */
export function previewConfigMigration(targets: ConfigMigrationTarget[]): ConfigMigrationPreview[] {
  const previews: ConfigMigrationPreview[] = [];
  for (const target of targets) {
    const raw = readPlainConfig(target.path);
    if (!raw) continue;
    const translated = structuredClone(raw);
    const translation = translateConfigDocument(translated, "window", target.tier);
    const retiredGithub =
      Object.hasOwn(raw, "gh_read") || (isRecord(raw.gh_shim) && "enabled" in raw.gh_shim);
    if (!translation.legacyInput && !retiredGithub) continue;
    const changes = diffConfig(raw, translated);
    // Keys the policy rejects outright are not translated, so the diff above
    // cannot show them; name the replacement the migration moves them to.
    for (const code of translation.errors) {
      const match = /^removed_config_key:([^:]+):use:(.+)$/.exec(code);
      if (match)
        changes.push({
          kind: "changed",
          key: match[1] as string,
          before: undefined,
          after: `→ ${match[2]}`,
        });
    }
    previews.push({ ...target, changes });
  }
  return previews;
}

function show(value: unknown): string {
  return JSON.stringify(value);
}

/** One readable line per change, e.g. `removed tool_surface ("recommended")`. */
export function describeConfigChange(change: ConfigChange): string {
  if (change.kind === "removed") return `removed ${change.key} (was ${show(change.before)})`;
  if (change.kind === "added") return `added ${change.key}: ${show(change.after)}`;
  if (
    change.before === undefined &&
    typeof change.after === "string" &&
    change.after.startsWith("→ ")
  ) {
    return `replaced ${change.key} with ${change.after.slice(2)}`;
  }
  return `changed ${change.key}: ${show(change.before)} → ${show(change.after)}`;
}
