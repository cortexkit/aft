import { existsSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { compareVersionLabels, dirSize } from "./fs-util.js";
import { getAftBinaryCacheDir } from "./paths.js";

export interface BinaryCacheInfo {
  versions: string[];
  activeVersion: string | null;
  totalSize: number;
  path: string;
}

/** Inspect ~/.cache/aft/bin/ for cached version dirs and total disk usage. */
export function getBinaryCacheInfo(activeVersion?: string): BinaryCacheInfo {
  const path = getAftBinaryCacheDir();
  if (!existsSync(path)) {
    return {
      versions: [],
      activeVersion: null,
      totalSize: 0,
      path,
    };
  }

  const versions = readdirSync(path)
    .filter((entry) => {
      try {
        return statSync(join(path, entry)).isDirectory();
      } catch {
        return false;
      }
    })
    .sort(compareVersionLabels);

  const tag = activeVersion
    ? activeVersion.startsWith("v")
      ? activeVersion
      : `v${activeVersion}`
    : null;
  const resolvedActive = tag && versions.includes(tag) ? tag : null;

  return {
    versions,
    activeVersion: resolvedActive,
    totalSize: dirSize(path),
    path,
  };
}
