/**
 * Find executables on PATH by checking each directory in-process.
 *
 * Shelling out to `which` / `where` costs a child process per lookup and, on
 * Windows, a console window whenever the host has none; reading the
 * directories directly avoids both.
 */

import { accessSync, constants, statSync } from "node:fs";
import { delimiter, resolve } from "node:path";

export interface PathLookupOptions {
  /** PATH value to search. Defaults to `process.env.PATH`. */
  pathValue?: string;
  platform?: NodeJS.Platform;
}

function executableNames(name: string, platform: NodeJS.Platform): string[] {
  if (platform !== "win32") return [name];
  return [name, `${name}.exe`, `${name}.cmd`, `${name}.bat`];
}

/**
 * Every file named `name` (plus the `.exe`/`.cmd`/`.bat` variants on Windows)
 * found on PATH, in PATH order and without duplicates. On POSIX a hit must be
 * executable by the current user; Windows has no execute bit, so existence is
 * enough there.
 */
export function findExecutablesOnPath(name: string, options: PathLookupOptions = {}): string[] {
  const platform = options.platform ?? process.platform;
  const pathValue = options.pathValue ?? process.env.PATH ?? "";
  const pathDelimiter = platform === "win32" ? ";" : delimiter;
  const seen = new Set<string>();
  const hits: string[] = [];
  for (const directory of pathValue.split(pathDelimiter)) {
    if (!directory) continue;
    for (const candidateName of executableNames(name, platform)) {
      const candidate = resolve(directory, candidateName);
      if (seen.has(candidate)) continue;
      seen.add(candidate);
      try {
        accessSync(candidate, platform === "win32" ? constants.F_OK : constants.X_OK);
        if (statSync(candidate).isFile()) hits.push(candidate);
      } catch {
        // Not here; keep looking through PATH.
      }
    }
  }
  return hits;
}

/** The first match `findExecutablesOnPath` would return, or null. */
export function findExecutableOnPath(name: string, options: PathLookupOptions = {}): string | null {
  return findExecutablesOnPath(name, options)[0] ?? null;
}
