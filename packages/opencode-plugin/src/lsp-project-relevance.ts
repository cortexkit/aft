import { type Dirent, existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const MAX_WALK_DIRS = 200;
const MAX_WALK_DEPTH = 4;

const NOISE_DIRS = new Set([
  ".git",
  ".next",
  ".venv",
  "__pycache__",
  "build",
  "dist",
  "node_modules",
  "target",
]);

export function hasRootMarker(projectRoot: string, rootMarkers?: readonly string[]): boolean {
  if (!rootMarkers) return false;
  for (const marker of rootMarkers) {
    if (existsSync(join(projectRoot, marker))) return true;
  }
  return false;
}

/**
 * True when `<projectRoot>/package.json` lists any name from `depNames`
 * in its `dependencies`, `devDependencies`, or `peerDependencies` maps.
 *
 * GitHub issue #48: Vue/Astro/Svelte projects fail the bounded extension
 * walk for monorepo layouts. Detecting them by package.json dep name
 * catches Vite-based setups and other frameworks where no framework-
 * specific config file exists at the project root.
 *
 * Reads package.json once per call. Failures (missing file, invalid JSON,
 * I/O error) return false — this signal is additive, never blocking.
 */
export function hasPackageJsonDep(projectRoot: string, depNames?: readonly string[]): boolean {
  if (!depNames || depNames.length === 0) return false;
  const pkg = readPackageJson(projectRoot);
  if (!pkg) return false;
  const merged: Record<string, unknown> = {
    ...(typeof pkg.dependencies === "object" && pkg.dependencies ? pkg.dependencies : {}),
    ...(typeof pkg.devDependencies === "object" && pkg.devDependencies ? pkg.devDependencies : {}),
    ...(typeof pkg.peerDependencies === "object" && pkg.peerDependencies
      ? pkg.peerDependencies
      : {}),
  };
  for (const name of depNames) {
    if (Object.hasOwn(merged, name)) return true;
  }
  return false;
}

interface PartialPackageJson {
  dependencies?: unknown;
  devDependencies?: unknown;
  peerDependencies?: unknown;
}

function readPackageJson(projectRoot: string): PartialPackageJson | null {
  try {
    const raw = readFileSync(join(projectRoot, "package.json"), "utf8");
    const parsed = JSON.parse(raw) as unknown;
    if (typeof parsed !== "object" || parsed === null) return null;
    return parsed as PartialPackageJson;
  } catch {
    return null;
  }
}

/**
 * Bounded extension scan for project relevance decisions.
 *
 * Root-marker checks happen before callers use this helper. This walk only
 * answers "does this project contain one of the extensions we know how to
 * serve?" and deliberately skips common dependency/build/cache directories so
 * vendored files do not trigger heavyweight LSP installs.
 */
export function relevantExtensionsInProject(
  projectRoot: string,
  extToServer: Readonly<Record<string, readonly string[]>>,
): Set<string> {
  const wanted = new Set(Object.keys(extToServer).map((ext) => ext.toLowerCase()));
  const found = new Set<string>();
  if (wanted.size === 0) return found;

  const queue: Array<{ dir: string; depth: number }> = [{ dir: projectRoot, depth: 0 }];
  let visitedDirs = 0;

  while (queue.length > 0 && visitedDirs < MAX_WALK_DIRS) {
    const current = queue.shift();
    if (!current) break;
    visitedDirs += 1;

    let entries: Dirent[];
    try {
      entries = readdirSync(current.dir, { withFileTypes: true });
    } catch {
      continue;
    }

    for (const entry of entries) {
      if (entry.isDirectory()) {
        if (current.depth < MAX_WALK_DEPTH && !NOISE_DIRS.has(entry.name.toLowerCase())) {
          queue.push({ dir: join(current.dir, entry.name), depth: current.depth + 1 });
        }
        continue;
      }

      if (!entry.isFile()) continue;
      const ext = extensionOf(entry.name);
      if (ext && wanted.has(ext)) found.add(ext);
    }
  }

  return found;
}

function extensionOf(fileName: string): string | null {
  const dot = fileName.lastIndexOf(".");
  if (dot < 0 || dot === fileName.length - 1) return null;
  return fileName.slice(dot + 1).toLowerCase();
}
