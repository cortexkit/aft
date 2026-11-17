import {
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import type { LegacyAftConfigSource, MigrationHarness } from "./paths.js";

export interface AftConfigFileMigrationOptions {
  scope: "user" | "project";
  targetPath: string;
  legacySources: readonly LegacyAftConfigSource[];
  /**
   * The harness whose plugin is running this migration. When two harnesses
   * (OpenCode + Pi) have DIFFERENT legacy configs, this one wins: its config
   * becomes the shared CortexKit target, and the other's is preserved beside it
   * as `<target>.<harness>_OLD` for manual merge. Without it, the first source
   * by list order wins (back-compat for callers that don't pass it).
   */
  operatingHarness?: MigrationHarness;
  logger?: {
    warn?: (msg: string) => void;
    info?: (msg: string) => void;
    log?: (msg: string) => void;
  };
}

export interface AftConfigFileMigrationResult {
  migrated: boolean;
  conflict: boolean;
  sourcePath?: string;
  targetPath: string;
  warnings: string[];
}

function stripJsoncForParse(input: string): string {
  let out = "";
  let inString = false;
  let escaped = false;
  for (let i = 0; i < input.length; i++) {
    const ch = input[i];
    const next = input[i + 1];
    if (inString) {
      out += ch;
      if (escaped) {
        escaped = false;
      } else if (ch === "\\") {
        escaped = true;
      } else if (ch === '"') {
        inString = false;
      }
      continue;
    }
    if (ch === '"') {
      inString = true;
      out += ch;
      continue;
    }
    if (ch === "/" && next === "/") {
      while (i < input.length && input[i] !== "\n") i++;
      out += "\n";
      continue;
    }
    if (ch === "/" && next === "*") {
      i += 2;
      while (i < input.length && !(input[i] === "*" && input[i + 1] === "/")) i++;
      i++;
      out += " ";
      continue;
    }
    out += ch;
  }
  let withoutTrailingCommas = "";
  inString = false;
  escaped = false;
  for (let i = 0; i < out.length; i++) {
    const ch = out[i];
    if (inString) {
      withoutTrailingCommas += ch;
      if (escaped) {
        escaped = false;
      } else if (ch === "\\") {
        escaped = true;
      } else if (ch === '"') {
        inString = false;
      }
      continue;
    }
    if (ch === '"') {
      inString = true;
      withoutTrailingCommas += ch;
      continue;
    }
    if (ch === ",") {
      let j = i + 1;
      while (j < out.length && /\s/.test(out[j])) j++;
      if (out[j] === "}" || out[j] === "]") continue;
    }
    withoutTrailingCommas += ch;
  }
  return withoutTrailingCommas;
}

function sortJson(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(sortJson);
  if (value && typeof value === "object") {
    const sorted: Record<string, unknown> = {};
    for (const key of Object.keys(value as Record<string, unknown>).sort()) {
      sorted[key] = sortJson((value as Record<string, unknown>)[key]);
    }
    return sorted;
  }
  return value;
}

function normalizedJsoncSemantics(content: string): string {
  return JSON.stringify(sortJson(JSON.parse(stripJsoncForParse(content))));
}

function fileSemanticsMatch(a: string, b: string): boolean {
  try {
    return normalizedJsoncSemantics(a) === normalizedJsoncSemantics(b);
  } catch {
    return a === b;
  }
}

function sleepSync(ms: number): void {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

function acquireConfigMigrationLock(lockDir: string): () => void {
  const deadline = Date.now() + 30_000;
  while (true) {
    try {
      mkdirSync(lockDir, { recursive: false });
      return () => {
        try {
          rmSync(lockDir, { recursive: true, force: true });
        } catch {
          // best-effort lock cleanup
        }
      };
    } catch (err) {
      const code = (err as { code?: unknown })?.code;
      if (code !== "EEXIST") throw err;
      try {
        const ageMs = Date.now() - statSync(lockDir).mtimeMs;
        if (ageMs > 60_000) {
          rmSync(lockDir, { recursive: true, force: true });
          continue;
        }
      } catch {
        // If the lock disappeared between mkdir attempts, retry immediately.
      }
      if (Date.now() >= deadline) {
        throw new Error(`timed out waiting for config migration lock ${lockDir}`);
      }
      sleepSync(25);
    }
  }
}

function atomicCopyConfigFile(sourcePath: string, targetPath: string): void {
  mkdirSync(dirname(targetPath), { recursive: true });
  const tmpPath = join(
    dirname(targetPath),
    `.${basename(targetPath)}.${process.pid}.${Date.now()}.${Math.random().toString(16).slice(2)}.tmp`,
  );
  let fd: number | null = null;
  try {
    fd = openSync(tmpPath, "wx", 0o600);
    writeFileSync(fd, readFileSync(sourcePath));
    closeSync(fd);
    fd = null;
    renameSync(tmpPath, targetPath);
  } catch (err) {
    if (fd !== null) {
      try {
        closeSync(fd);
      } catch {
        // best-effort close before cleanup
      }
    }
    try {
      unlinkSync(tmpPath);
    } catch {
      // best-effort temp cleanup
    }
    throw err;
  }
}

function atomicWriteConfigFile(targetPath: string, content: string): void {
  mkdirSync(dirname(targetPath), { recursive: true });
  const tmpPath = join(
    dirname(targetPath),
    `.${basename(targetPath)}.${process.pid}.${Date.now()}.${Math.random().toString(16).slice(2)}.tmp`,
  );
  let fd: number | null = null;
  try {
    fd = openSync(tmpPath, "wx", 0o600);
    writeFileSync(fd, content);
    closeSync(fd);
    fd = null;
    renameSync(tmpPath, targetPath);
  } catch (err) {
    if (fd !== null) {
      try {
        closeSync(fd);
      } catch {
        // best-effort close before cleanup
      }
    }
    try {
      unlinkSync(tmpPath);
    } catch {
      // best-effort temp cleanup
    }
    throw err;
  }
}

const MOVED_MARKER_SUFFIX = ".MOVED_READPLEASE";

function nonClobberingSidecarPath(desiredPath: string): string {
  if (!existsSync(desiredPath)) return desiredPath;
  for (let index = 1; index < Number.MAX_SAFE_INTEGER; index++) {
    const candidate = `${desiredPath}.${index}`;
    if (!existsSync(candidate)) return candidate;
  }
  throw new Error(`could not find available preservation sidecar path for ${desiredPath}`);
}

function movedMarkerContent(
  targetPath: string,
  originalName: string,
  originalContent: string,
): string {
  const header = [
    "// AFT configuration moved.",
    "//",
    "// AFT now reads its configuration from one shared CortexKit location",
    "// instead of a per-agent path. The settings that were in this file have",
    "// been moved to:",
    "//",
    `//     ${targetPath}`,
    "//",
    "// Edit that file to change AFT settings. This location is no longer read",
    "// by AFT.",
    "//",
    `// To undo, rename this file back to "${originalName}" (and remove the`,
    "// CortexKit copy above if you want this location to take precedence).",
    "//",
    "// Your original settings are preserved below for reference.",
    "",
    "",
  ].join("\n");
  return `${header}${originalContent}`;
}

// After the live config is safely at the CortexKit target, rename each legacy
// source aside to a "<name>.MOVED_READPLEASE" marker so a user editing the old
// path notices it is no longer read (a copy-in-place leaves a silent stale-edit
// trap). The marker preserves the original settings and documents how to undo.
// A failure here is non-fatal: the content is already at the target, so we warn
// and leave the legacy file in place rather than failing the migration.
function markLegacySourcesMovedAside(
  sources: readonly { path: string }[],
  targetPath: string,
  logger?: AftConfigFileMigrationOptions["logger"],
): string[] {
  const warnings: string[] = [];
  const info = logger?.info ?? logger?.log;
  for (const source of sources) {
    const desiredMarkerPath = `${source.path}${MOVED_MARKER_SUFFIX}`;
    const markerPath = nonClobberingSidecarPath(desiredMarkerPath);
    try {
      const original = readFileSync(source.path, "utf-8");
      if (markerPath !== desiredMarkerPath) {
        logger?.warn?.(
          `Preserving existing legacy AFT marker ${desiredMarkerPath}; writing new marker to ${markerPath}`,
        );
      }
      atomicWriteConfigFile(
        markerPath,
        movedMarkerContent(targetPath, basename(source.path), original),
      );
      unlinkSync(source.path);
      info?.(
        `Moved legacy AFT config ${source.path} aside to ${markerPath}; AFT now reads ${targetPath}`,
      );
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      warnings.push(
        `AFT could not move legacy config ${source.path} aside (${msg}); it is now stale and ignored. Delete it manually — AFT reads ${targetPath}.`,
      );
      logger?.warn?.(
        `Could not move legacy AFT config ${source.path} aside (${msg}); AFT reads ${targetPath}`,
      );
    }
  }
  return warnings;
}

const PRESERVED_OLD_SUFFIX = "_OLD";

function preservedOldContent(
  losingHarness: MigrationHarness,
  winningHarness: MigrationHarness | undefined,
  targetPath: string,
  originalContent: string,
): string {
  const winnerLine = winningHarness
    ? `// configs differed, so ${winningHarness}'s config won (first harness to run the`
    : "// configs differed, so the config that was migrated first won and is the";
  const header = [
    `// AFT config from ${losingHarness} — preserved, NOT read.`,
    "//",
    "// AFT now reads one shared config for all harnesses. Your OpenCode and Pi",
    winnerLine,
    winningHarness ? "// migration) and is now the active config at:" : "// active config at:",
    "//",
    `//     ${targetPath}`,
    "//",
    `// This file holds ${losingHarness}'s previous settings. Merge anything you`,
    "// want to keep into the active config above, then delete this file. AFT does",
    "// not read this path.",
    "",
    "",
  ].join("\n");
  return `${header}${originalContent}`;
}

// When two harnesses have DIFFERENT legacy configs, the operating harness's
// config wins (becomes the shared target) and the other's is preserved beside
// the target as `<target>.<harness>_OLD` so the user can merge it. This replaces
// the old refuse-and-write-nothing behavior, which dropped the plugin to default
// config and could wipe a fingerprint-keyed semantic index. `winningHarness` is
// undefined when the target already existed (we don't know which harness wrote
// it first), in which case the message describes the existing shared config.
function preserveDifferingSourceAsOld(
  source: { path: string; content: string; harness?: MigrationHarness },
  winningHarness: MigrationHarness | undefined,
  targetPath: string,
  logger?: AftConfigFileMigrationOptions["logger"],
): string {
  const losingHarness = source.harness ?? "pi";
  const desiredOldPath = `${targetPath}.${losingHarness}${PRESERVED_OLD_SUFFIX}`;
  const oldPath = nonClobberingSidecarPath(desiredOldPath);
  try {
    if (oldPath !== desiredOldPath) {
      logger?.warn?.(
        `Preserving existing ${losingHarness} config sidecar ${desiredOldPath}; writing new preserved config to ${oldPath}`,
      );
    }
    atomicWriteConfigFile(
      oldPath,
      preservedOldContent(losingHarness, winningHarness, targetPath, source.content),
    );
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    logger?.warn?.(`Could not preserve ${losingHarness} config as ${oldPath} (${msg})`);
  }
  const usingDesc = winningHarness ? `${winningHarness}'s config` : "the existing shared config";
  return (
    `AFT found different OpenCode and Pi configs during config unification. ` +
    `Using ${usingDesc} at ${targetPath}; preserved ${losingHarness}'s previous ` +
    `config at ${oldPath} — merge any settings you want to keep, then delete it.`
  );
}

function visibleConfigMigrationWarning(
  scope: "user" | "project",
  targetPath: string,
  paths: readonly string[],
  reason: string,
): string {
  const uniquePaths = [...new Set([targetPath, ...paths])];
  return (
    `AFT ${scope} config migration refused: ${reason}. ` +
    `Legacy and CortexKit config paths collapse to one file, but AFT will not overwrite or merge them automatically. ` +
    `Please consolidate manually into ${targetPath}. Paths: ${uniquePaths.join(" ; ")}`
  );
}

export function migrateAftConfigFile(
  opts: AftConfigFileMigrationOptions,
): AftConfigFileMigrationResult {
  const warnings: string[] = [];
  // A legacy source path can resolve to the SAME file as the CortexKit target
  // (e.g. OPENCODE_CONFIG_DIR pointed at ~/.config/cortexkit, so the legacy
  // `<opencode-config>/aft.jsonc` IS `~/.config/cortexkit/aft.jsonc`). That file
  // is the live, already-migrated config — never a legacy source to copy from,
  // preserve, or (critically) `unlinkSync` aside. Dropping it here prevents the
  // active config from being deleted, which would fall the plugin back to
  // defaults and wipe the fingerprint-keyed semantic index.
  const resolvedTarget = resolve(opts.targetPath);
  const existingSources = opts.legacySources.filter(
    (source) => existsSync(source.path) && resolve(source.path) !== resolvedTarget,
  );
  const info = opts.logger?.info ?? opts.logger?.log;

  if (existingSources.length === 0) {
    return { migrated: false, conflict: false, targetPath: opts.targetPath, warnings };
  }

  mkdirSync(dirname(opts.targetPath), { recursive: true });
  const release = acquireConfigMigrationLock(`${opts.targetPath}.lock`);
  try {
    const sources = existingSources.map((source) => ({
      ...source,
      content: readFileSync(source.path, "utf-8"),
    }));

    // Target already exists: first-opened wins permanently. We never overwrite
    // it (whichever harness migrated first owns the shared config). Any legacy
    // source that DIFFERS from it is preserved beside the target as
    // `<target>.<harness>_OLD` for manual merge; matching ones are just cleared.
    if (existsSync(opts.targetPath)) {
      const targetContent = readFileSync(opts.targetPath, "utf-8");
      for (const source of sources) {
        if (fileSemanticsMatch(source.content, targetContent)) continue;
        warnings.push(
          preserveDifferingSourceAsOld(source, undefined, opts.targetPath, opts.logger),
        );
      }
      info?.(
        `AFT ${opts.scope} config already present at ${opts.targetPath}; reconciled ${sources.length} legacy source(s)`,
      );
      warnings.push(...markLegacySourcesMovedAside(sources, opts.targetPath, opts.logger));
      return { migrated: false, conflict: false, targetPath: opts.targetPath, warnings };
    }

    // Target does not exist yet. Pick the winner: the operating harness's source
    // if present, else the first source by list order (back-compat for callers
    // that don't pass operatingHarness). The winner's config becomes the shared
    // target; any OTHER source that differs is preserved as `<target>.<harness>_OLD`.
    const winner =
      (opts.operatingHarness
        ? sources.find((source) => source.harness === opts.operatingHarness)
        : undefined) ?? sources[0];

    atomicCopyConfigFile(winner.path, opts.targetPath);
    info?.(`Migrated AFT ${opts.scope} config from ${winner.path} to ${opts.targetPath}`);

    for (const source of sources) {
      if (source.path === winner.path) continue;
      if (fileSemanticsMatch(source.content, winner.content)) continue;
      warnings.push(
        preserveDifferingSourceAsOld(source, winner.harness, opts.targetPath, opts.logger),
      );
    }

    warnings.push(...markLegacySourcesMovedAside(sources, opts.targetPath, opts.logger));
    return {
      migrated: true,
      conflict: false,
      sourcePath: winner.path,
      targetPath: opts.targetPath,
      warnings,
    };
  } catch (err) {
    const message = visibleConfigMigrationWarning(
      opts.scope,
      opts.targetPath,
      existingSources.map((source) => source.path),
      `migration failed (${err instanceof Error ? err.message : String(err)})`,
    );
    warnings.push(message);
    opts.logger?.warn?.(message);
    return { migrated: false, conflict: true, targetPath: opts.targetPath, warnings };
  } finally {
    release();
  }
}
