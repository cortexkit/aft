import { existsSync, readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import type { PluginCacheInfo } from "../adapters/types.js";
import {
  AFT_OPENCODE_PACKAGE,
  exactPinnedVersion,
  type OpenCodeConfigGeneration,
} from "../setup/opencode-config.js";
import { compareSemver } from "./semver.js";

export function discoverLocalOpenCodePlugin(
  path: string,
  cliVersion: string,
): PluginCacheInfo | undefined {
  let current = path;
  for (;;) {
    const manifestPath = join(current, "package.json");
    try {
      const pkg = JSON.parse(readFileSync(manifestPath, "utf8"));
      if (pkg.name === AFT_OPENCODE_PACKAGE && typeof pkg.version === "string") {
        return {
          path: current,
          manifestPath,
          cached: pkg.version,
          version: pkg.version,
          versionSource: "installed",
          exists: true,
          cliVersion,
          local: true,
        };
      }
    } catch {
      /* An entry may name a source file below the package root. */
    }
    const parent = dirname(current);
    if (parent === current) return undefined;
    current = parent;
  }
}

/** Discover only installations for the configured entry and the selected host generation. */
export function discoverOpenCodePluginCache(
  root: string,
  entry: string,
  generation: OpenCodeConfigGeneration,
  cliVersion: string,
  configuredEntry?: string,
): PluginCacheInfo {
  const v1 = join(root, "packages", entry);
  const v2 = join(root, "npm", entry);
  const paths: string[] = [];
  if (generation !== "v1") {
    try {
      // OpenCode creates a new numeric timestamp for each resolution. Tags use
      // resolution recency, not the highest package version in the cache.
      const timestamps = readdirSync(v2).filter((name) => /^\d+$/.test(name));
      timestamps.sort((a, b) => (BigInt(a) > BigInt(b) ? -1 : BigInt(a) < BigInt(b) ? 1 : 0));
      // The host selects the newest numeric generation even when it is
      // incomplete. Never report an older generation as the active install.
      if (timestamps[0]) paths.push(join(v2, timestamps[0]));
    } catch {
      // No completed V2 installation has been discovered yet.
    }
  }
  if (generation === "v1" || (generation !== "v2" && paths.length === 0)) paths.push(v1);
  let readError: string | undefined;
  for (const path of paths) {
    const manifestPath = join(path, "node_modules", AFT_OPENCODE_PACKAGE, "package.json");
    try {
      const pkg = JSON.parse(readFileSync(manifestPath, "utf8"));
      if (
        pkg.name !== AFT_OPENCODE_PACKAGE ||
        typeof pkg.version !== "string" ||
        compareSemver(pkg.version, pkg.version) !== 0
      ) {
        readError = `Invalid plugin identity or version at ${manifestPath}`;
        continue;
      }
      // A manifest with a declared but missing entrypoint is an incomplete
      // install, not evidence of the version the host can load.
      if (
        typeof pkg.main === "string" &&
        !existsSync(join(path, "node_modules", AFT_OPENCODE_PACKAGE, pkg.main))
      ) {
        readError = `Incomplete plugin installation at ${manifestPath}`;
        continue;
      }
      return {
        path,
        manifestPath,
        cached: pkg.version,
        version: pkg.version,
        versionSource: "installed",
        configuredEntry,
        exists: true,
        cliVersion,
      };
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT" || existsSync(path))
        readError = String(error);
    }
  }
  return {
    path: paths[0] ?? (generation === "v2" ? v2 : v1),
    cached: undefined,
    version: configuredEntry ? (exactPinnedVersion(configuredEntry) ?? undefined) : undefined,
    versionSource:
      configuredEntry && exactPinnedVersion(configuredEntry) ? "configured" : "unknown",
    readError,
    exists: false,
    configuredEntry,
    cliVersion,
  };
}

/** An unreadable current install cannot establish that replacing its version is safe. */
export function doctorInstalledVersion(cache: PluginCacheInfo): string | undefined {
  return cache.cached ?? (cache.readError ? "unknown" : undefined);
}

export function describePluginVersion(cache: PluginCacheInfo): string {
  const version = cache.version ?? cache.cached;
  const source = cache.versionSource ?? (cache.cached ? "installed" : "unknown");
  return `${version ?? "unknown"} (${source}${cache.manifestPath ? `: ${cache.manifestPath}` : source === "configured" && cache.configuredEntry ? `: ${cache.configuredEntry}` : ""})`;
}
