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
  statSync,
  symlinkSync,
  unlinkSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

const cacheRoot = join(tmpdir(), "aft-test-executables");
const retentionMs = 24 * 60 * 60 * 1000;

function refresh(directory: string): void {
  const now = new Date();
  utimesSync(directory, now, now);
}

export function pruneExecutableCache(
  root: string,
  current: string,
  now = Date.now(),
  claim: (source: string, destination: string) => void = renameSync,
  cleanup: (directory: string) => void = (directory) =>
    rmSync(directory, { recursive: true, force: true }),
): void {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const directory = join(root, entry.name);
    if (directory === current || !entry.isDirectory() || !/^[a-f0-9]{64}$/.test(entry.name))
      continue;
    let quarantine: string | undefined;
    let preserve = false;
    let failure: { error: unknown } | undefined;
    try {
      if (now - statSync(directory).mtimeMs <= retentionMs) continue;
      quarantine = mkdtempSync(join(root, ".pruning-"));
      const removed = join(quarantine, "image");
      // Recheck after claiming the pathname: a concurrent reader may have
      // refreshed it after the scan, or may republish while it is absent.
      claim(directory, removed);
      if (now - statSync(removed).mtimeMs <= retentionMs) {
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
): string {
  try {
    return publishExecutable(bytes, root, name);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    // A pruner may have claimed an old identity during lookup. Republish once.
    return publishExecutable(bytes, root, name);
  }
}

function publishExecutable(bytes: Uint8Array, root: string, name: string): string {
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
  pruneExecutableCache(root, dirname(path));
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
