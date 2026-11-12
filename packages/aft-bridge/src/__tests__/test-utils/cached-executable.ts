import { createHash, randomUUID } from "node:crypto";
import {
  chmodSync,
  linkSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  renameSync,
  rmSync,
  statfsSync,
  statSync,
  symlinkSync,
  unlinkSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { type CacheUsage, cacheUsageSnapshot, pidAlive } from "./image-cache-usage.js";

const cacheRoot = join(tmpdir(), "aft-test-executables");
const retentionMs = 24 * 60 * 60 * 1000;
const pressureRetentionMs = 30 * 60 * 1000;
const leaseRetentionMs = 3 * 60 * 60 * 1000;
const freeFloor = 40 * 1024 ** 3;

export interface CachePruneProbes {
  usage?: CacheUsage;
  freeBytes?: (root: string) => number;
}

function refresh(directory: string): void {
  const now = new Date();
  const leases = join(directory, ".leases");
  mkdirSync(leases, { recursive: true });
  writeFileSync(join(leases, String(process.pid)), "");
  utimesSync(directory, now, now);
}

function leased(directory: string, now: number, usage: CacheUsage): boolean {
  const leases = join(directory, ".leases");
  let entries: string[];
  try {
    entries = readdirSync(leases);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return false;
    throw error;
  }
  let active = false;
  for (const name of entries) {
    if (!/^[1-9]\d*$/.test(name)) continue;
    const path = join(leases, name);
    if (now - statSync(path).mtimeMs < leaseRetentionMs && usage.pidAlive(Number(name)))
      active = true;
    else rmSync(path, { force: true });
  }
  return active;
}

export function pruneExecutableCache(
  root: string,
  current: string,
  now = Date.now(),
  claim: (source: string, destination: string) => void = renameSync,
  cleanup: (directory: string) => void = (directory) =>
    rmSync(directory, { recursive: true, force: true }),
  probes: CachePruneProbes = {},
): void {
  let free = Number.POSITIVE_INFINITY;
  try {
    free = probes.freeBytes
      ? probes.freeBytes(root)
      : Number(statfsSync(root).bavail) * Number(statfsSync(root).bsize);
  } catch {
    /* Count and normal age limits still apply when free space is unknown. */
  }
  const entries = readdirSync(root, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && /^[a-f0-9]{64}$/.test(entry.name))
    .flatMap((entry) => {
      const directory = join(root, entry.name);
      try {
        return [{ directory, modified: statSync(directory).mtimeMs }];
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code === "ENOENT") return [];
        throw error;
      }
    })
    .sort((a, b) => b.modified - a.modified || a.directory.localeCompare(b.directory));
  const leaseUsage: CacheUsage = probes.usage ?? { pidAlive, inUse: () => "absent" };
  const candidates = entries.filter((entry) => {
    try {
      const active = leased(entry.directory, now, leaseUsage);
      return entry.directory !== current && !active;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return false;
      throw error;
    }
  });
  if (
    candidates.length <= 4 &&
    !candidates.some(({ modified }) => {
      const age = now - modified;
      return age > retentionMs || (free < freeFloor && age > pressureRetentionMs);
    })
  )
    return;
  let usage: CacheUsage;
  try {
    usage = probes.usage ?? cacheUsageSnapshot();
  } catch {
    return;
  }
  let unused = 0;
  for (const { directory, modified } of entries) {
    if (directory === current) continue;
    let quarantine: string | undefined;
    let preserve = false;
    let failure: { error: unknown } | undefined;
    try {
      if (leased(directory, now, usage) || usage.inUse(directory) !== "absent") continue;
      const age = now - modified;
      if (unused++ < 4 && age <= retentionMs && !(free < freeFloor && age > pressureRetentionMs))
        continue;
      quarantine = mkdtempSync(join(root, ".pruning-"));
      const removed = join(quarantine, "image");
      // Recheck after claiming the pathname: a concurrent reader may have
      // refreshed it after the scan, or may republish while it is absent.
      claim(directory, removed);
      let retain = true;
      try {
        retain =
          statSync(removed).mtimeMs > modified ||
          leased(removed, now, usage) ||
          usage.inUse(removed) !== "absent";
      } catch {
        // Uncertain ownership must restore the claimed image, not delete it.
      }
      if (retain) {
        try {
          renameSync(removed, directory);
        } catch (error) {
          try {
            if (!statSync(directory).isDirectory()) throw error;
          } catch {
            preserve = true;
            throw error;
          }
        }
      }
    } catch (error) {
      const code = (error as NodeJS.ErrnoException).code;
      // Another pruner can win the claim; Windows can also refuse to move a
      // running executable. Neither should fail an unrelated fixture lookup.
      if (code !== "ENOENT" && code !== "EACCES" && code !== "EPERM" && code !== "EBUSY")
        failure = { error };
    }
    if (quarantine && !preserve) {
      try {
        cleanup(quarantine);
      } catch (error) {
        const code = (error as NodeJS.ErrnoException).code;
        if (code !== "EACCES" && code !== "EPERM" && code !== "EBUSY" && !failure)
          failure = { error };
      }
    }
    if (failure) throw failure.error;
  }
}

export function cachedExecutable(source: string): string {
  return cachedExecutableBytes(Buffer.from(source));
}

export function cachedExecutableBytes(
  bytes: Uint8Array,
  root = cacheRoot,
  name = process.platform === "win32" ? "executable.exe" : "executable",
  probes: CachePruneProbes = {},
): string {
  try {
    return publishExecutable(bytes, root, name, probes);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    // A pruner may have claimed an old identity during lookup. Republish once.
    return publishExecutable(bytes, root, name, probes);
  }
}

function publishExecutable(
  bytes: Uint8Array,
  root: string,
  name: string,
  probes: CachePruneProbes,
): string {
  const digest = createHash("sha256").update("mode:755\0").update(bytes).digest("hex");
  const path = join(root, digest, name);
  try {
    const stat = statSync(path);
    if (!stat.isFile() || (stat.mode & 0o777) !== 0o755 || !readFileSync(path).equals(bytes)) {
      throw new Error(`content-addressed test executable is invalid: ${path}`);
    }
    refresh(dirname(path));
    return path;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
  }

  mkdirSync(dirname(path), { recursive: true });
  // Link publication is atomic and never replaces a path another process won.
  const pending = join(dirname(path), `.pending-${process.pid}-${randomUUID()}`);
  try {
    writeFileSync(pending, bytes, { flag: "wx" });
    chmodSync(pending, 0o755);
    try {
      linkSync(pending, path);
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
      const stat = statSync(path);
      if (!stat.isFile() || (stat.mode & 0o777) !== 0o755 || !readFileSync(path).equals(bytes)) {
        throw new Error(`content-addressed test executable is invalid: ${path}`);
      }
    }
  } finally {
    try {
      unlinkSync(pending);
    } catch {
      // A failed best-effort cleanup must not hide the original publication error.
    }
  }
  refresh(dirname(path));
  pruneExecutableCache(root, dirname(path), Date.now(), undefined, undefined, probes);
  return path;
}

export function linkCachedExecutable(
  path: string,
  source: string,
  root = cacheRoot,
  link: (source: string, destination: string) => void = symlinkSync,
): string {
  mkdirSync(dirname(path), { recursive: true });
  for (let attempt = 0; attempt < 2; attempt++) {
    const cached = cachedExecutableBytes(Buffer.from(source), root);
    try {
      link(cached, path);
      // A symlink can be created successfully even if its target disappeared.
      statSync(cached);
      return path;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT" || attempt !== 0) throw error;
      rmSync(path, { force: true });
    }
  }
  throw new Error("unreachable executable cache retry");
}
