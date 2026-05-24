import { existsSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

/** Recursive byte size of a file or directory; 0 when missing. */
export function dirSize(path: string): number {
  if (!existsSync(path)) {
    return 0;
  }

  const stat = statSync(path);
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
  if (bytes === 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.floor(Math.log(bytes) / Math.log(1024));
  return `${(bytes / 1024 ** i).toFixed(1)} ${units[i]}`;
}

/** Locale-aware numeric version label comparison (`v0.10.0` > `v0.9.0`). */
export function compareVersionLabels(a: string, b: string): number {
  return a.localeCompare(b, undefined, { numeric: true, sensitivity: "base" });
}
