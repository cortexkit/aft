import { createHash, randomUUID } from "node:crypto";
import {
  chmodSync,
  linkSync,
  mkdirSync,
  readFileSync,
  renameSync,
  statSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

const cacheRoot = join(tmpdir(), "aft-test-executables");

export function cachedExecutable(source: string): string {
  const digest = createHash("sha256").update("mode:755\0").update(source).digest("hex");
  const path = join(cacheRoot, digest, "executable");
  try {
    const stat = statSync(path);
    if (stat.isFile() && (stat.mode & 0o777) === 0o755 && readFileSync(path, "utf8") === source)
      return path;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
  }

  mkdirSync(dirname(path), { recursive: true });
  // Publish only complete executable bytes; another test process may use the same digest.
  const pending = join(dirname(path), `.pending-${process.pid}-${randomUUID()}`);
  try {
    writeFileSync(pending, source);
    chmodSync(pending, 0o755);
    renameSync(pending, path);
  } finally {
    try {
      unlinkSync(pending);
    } catch {
      // A failed best-effort cleanup must not hide the original publication error.
    }
  }
  return path;
}

// A hard link retains the fixture's per-test realpath when resolution inspects
// neighboring package metadata; it reuses the cached inode instead of copying it.
export function hardlinkCachedExecutable(path: string, source: string): string {
  mkdirSync(dirname(path), { recursive: true });
  linkSync(cachedExecutable(source), path);
  return path;
}

export function linkCachedExecutable(path: string, source: string): string {
  mkdirSync(dirname(path), { recursive: true });
  symlinkSync(cachedExecutable(source), path);
  return path;
}
