/**
 * Inspection and cleanup helpers for the AFT plugin's LSP cache.
 *
 * Two roots are populated by the plugin auto-installer at startup:
 *
 *   <aft-cache-root>/lsp-packages/<urlencoded-pkg>/   ← npm installs
 *   <aft-cache-root>/lsp-binaries/<id>/               ← GitHub-release downloads
 *
 * The CLI reports their disk usage in `aft doctor` and can purge them via
 * `aft doctor --force`. Purging is non-destructive in the sense that the
 * plugin will simply re-install whatever the project needs on next session;
 * it does not delete user-installed binaries (those live on PATH).
 */

import { existsSync, readdirSync, rmSync, statSync } from "node:fs";
import { join } from "node:path";
import { dirSize } from "./fs-util.js";
import { getAftLspBinariesDir, getAftLspPackagesDir } from "./paths.js";

export interface LspCacheEntry {
  /** Display name (npm package name or GitHub server id). */
  name: string;
  /** Absolute path to the entry root. */
  path: string;
  /** Total bytes occupied by this entry. */
  size: number;
}

export interface LspCacheReport {
  /** npm-installed servers (`<root>/lsp-packages/`). */
  npm: {
    path: string;
    entries: LspCacheEntry[];
    totalSize: number;
  };
  /** GitHub-installed servers (`<root>/lsp-binaries/`). */
  github: {
    path: string;
    entries: LspCacheEntry[];
    totalSize: number;
  };
  /** Combined size of both subtrees. Convenience for the doctor display. */
  totalSize: number;
}

function inspectDir(path: string): {
  entries: LspCacheEntry[];
  totalSize: number;
} {
  if (!existsSync(path)) {
    return { entries: [], totalSize: 0 };
  }
  const entries: LspCacheEntry[] = [];
  let totalSize = 0;

  let names: string[];
  try {
    names = readdirSync(path);
  } catch {
    return { entries: [], totalSize: 0 };
  }

  for (const name of names) {
    const full = join(path, name);
    try {
      if (!statSync(full).isDirectory()) continue;
      const size = dirSize(full);
      entries.push({
        // Reverse the URL-encoding the plugin applies so users see real names.
        name: decodeURIComponent(name),
        path: full,
        size,
      });
      totalSize += size;
    } catch {
      // ignore — broken symlinks, permissions, etc.
    }
  }

  // Sort by size desc so doctor output highlights the heaviest installs.
  entries.sort((a, b) => b.size - a.size);

  return { entries, totalSize };
}

/** Build a full inspection report for both LSP cache subtrees. */
export function getLspCacheReport(): LspCacheReport {
  const npmPath = getAftLspPackagesDir();
  const githubPath = getAftLspBinariesDir();
  const npm = inspectDir(npmPath);
  const github = inspectDir(githubPath);
  return {
    npm: {
      path: npmPath,
      entries: npm.entries,
      totalSize: npm.totalSize,
    },
    github: {
      path: githubPath,
      entries: github.entries,
      totalSize: github.totalSize,
    },
    totalSize: npm.totalSize + github.totalSize,
  };
}

export interface ClearResult {
  cleared: { name: string; path: string; size: number }[];
  errors: { path: string; error: string }[];
  totalBytes: number;
}

/**
 * Remove every entry under both LSP cache subtrees.
 *
 * Returns metadata for the doctor output (number of entries cleared, bytes
 * reclaimed, individual failures). Errors don't abort — we want to clean
 * as much as possible even when some directories are locked or have unusual
 * permissions.
 */
export function clearLspCaches(): ClearResult {
  const result: ClearResult = { cleared: [], errors: [], totalBytes: 0 };
  const report = getLspCacheReport();

  for (const entry of [...report.npm.entries, ...report.github.entries]) {
    try {
      rmSync(entry.path, { recursive: true, force: true });
      result.cleared.push({ name: entry.name, path: entry.path, size: entry.size });
      result.totalBytes += entry.size;
    } catch (err) {
      result.errors.push({
        path: entry.path,
        error: err instanceof Error ? err.message : String(err),
      });
    }
  }

  return result;
}
