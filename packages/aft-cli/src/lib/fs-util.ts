import { existsSync, lstatSync, readdirSync } from "node:fs";
import { join } from "node:path";

/** Physical byte size of a file or directory; symlink targets are excluded. */
export function dirSize(path: string): number {
  if (!existsSync(path)) {
    return 0;
  }

  const stat = lstatSync(path);
  if (stat.isFile()) {
    return stat.size;
  }
  if (!stat.isDirectory()) {
    return 0;
  }

  let total = 0;
  for (const entry of readdirSync(path)) {
    total += dirSize(join(path, entry));
  }
  return total;
}

/** Human-readable file size: 12.3 KB / 4.5 MB / 1.2 GB. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 1) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  return `${(bytes / 1024 ** i).toFixed(1)} ${units[i]}`;
}

/** Locale-aware numeric version label comparison (`v0.10.0` > `v0.9.0`). */
export function compareVersionLabels(a: string, b: string): number {
  return a.localeCompare(b, undefined, { numeric: true, sensitivity: "base" });
}
