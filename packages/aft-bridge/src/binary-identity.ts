/**
 * Identity sidecars for binaries in AFT's versioned cache.
 *
 * Plugin hosts (OpenCode, Pi) run every plugin on one JavaScript thread. Asking
 * a cached `aft` binary for its version means executing it, and on macOS the
 * host thread stays inside `posix_spawn` until the child image is paged in and
 * assessed. For an 85 MB binary under memory pressure that has been measured
 * at close to a minute, and the same file goes cold again during the day, so
 * the cost is not paid only once. The host froze for that long on every
 * instance boot.
 *
 * Instead of executing the binary, every writer of the versioned cache records
 * what it installed in a small JSON file next to the binary
 * (`<binary>.identity.json`): the version, byte size, modification time, inode
 * and a SHA-256 of the bytes. Resolution then trusts a cached binary when its
 * sidecar exists and the file's size, mtime and inode still match — a `stat`
 * and a tiny read, never an exec and never a hash. Any mismatch (the file was
 * replaced, touched, or copied over) makes the entry untrusted again.
 *
 * The SHA-256 is never computed on the resolve path, because hashing 85 MB
 * synchronously would block the host thread just like the exec did. It is
 * what a running bridge compares against later to notice that the binary on
 * disk was replaced (see {@link peekBinaryContentHash}).
 */

import { createHash } from "node:crypto";
import {
  createReadStream,
  readFileSync,
  renameSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { warn } from "./active-logger.js";

const SIDECAR_SCHEMA = 1;

/** Contents of a binary identity sidecar. Numbers that can exceed 2^53 are strings. */
export interface BinaryIdentity {
  schema: typeof SIDECAR_SCHEMA;
  /** Bare version (no leading `v`), e.g. `"0.57.2"`. */
  version: string;
  size: string;
  /** Modification time in nanoseconds since the epoch. */
  mtimeNs: string;
  /** Inode number, or null where the platform does not report a stable one. */
  ino: string | null;
  sha256: string;
}

export type BinaryIdentityCheck =
  | { status: "trusted"; version: string }
  | { status: "missing" }
  | { status: "mismatch"; reason: string };

interface FileStamp {
  size: string;
  mtimeNs: string;
  ino: string | null;
}

function bareVersion(version: string): string {
  return version.startsWith("v") ? version.slice(1) : version;
}

/** Path of the identity sidecar for `binaryPath`. */
export function identitySidecarPath(binaryPath: string): string {
  return `${binaryPath}.identity.json`;
}

function stampOf(binaryPath: string): FileStamp {
  const stat = statSync(binaryPath, { bigint: true });
  // Some filesystems (notably FAT volumes on Windows) report inode 0; treat it
  // as unknown rather than as a value every file shares.
  const ino = stat.ino > 0n ? stat.ino.toString() : null;
  return { size: stat.size.toString(), mtimeNs: stat.mtimeNs.toString(), ino };
}

function sameStamp(a: FileStamp, b: FileStamp): boolean {
  return a.size === b.size && a.mtimeNs === b.mtimeNs && a.ino === b.ino;
}

function parseIdentity(raw: string): BinaryIdentity | null {
  try {
    const value = JSON.parse(raw) as Partial<BinaryIdentity>;
    if (
      value.schema !== SIDECAR_SCHEMA ||
      typeof value.version !== "string" ||
      typeof value.size !== "string" ||
      typeof value.mtimeNs !== "string" ||
      !(value.ino === null || typeof value.ino === "string") ||
      typeof value.sha256 !== "string"
    ) {
      return null;
    }
    return value as BinaryIdentity;
  } catch {
    return null;
  }
}

/** Read and validate the sidecar for `binaryPath`; null when absent or malformed. */
export function readBinaryIdentity(binaryPath: string): BinaryIdentity | null {
  let raw: string;
  try {
    raw = readFileSync(identitySidecarPath(binaryPath), "utf8");
  } catch {
    return null;
  }
  return parseIdentity(raw);
}

/**
 * Decide whether the binary at `binaryPath` is still the file its sidecar
 * describes. Uses `stat` only: no exec and no hash, so it is safe on a host's
 * main thread.
 */
export function checkBinaryIdentity(binaryPath: string): BinaryIdentityCheck {
  const identity = readBinaryIdentity(binaryPath);
  if (!identity) return { status: "missing" };
  let stamp: FileStamp;
  try {
    stamp = stampOf(binaryPath);
  } catch {
    return { status: "mismatch", reason: "binary is missing or unreadable" };
  }
  if (stamp.size !== identity.size) {
    return { status: "mismatch", reason: `size ${stamp.size} != recorded ${identity.size}` };
  }
  if (stamp.mtimeNs !== identity.mtimeNs) {
    return { status: "mismatch", reason: `mtime ${stamp.mtimeNs} != recorded ${identity.mtimeNs}` };
  }
  // A sidecar written where the inode was unknown cannot vouch for it either
  // way; size and mtime still have to match.
  if (identity.ino !== null && stamp.ino !== identity.ino) {
    return { status: "mismatch", reason: `inode ${stamp.ino} != recorded ${identity.ino}` };
  }
  return { status: "trusted", version: identity.version };
}

/**
 * True when `binaryPath` has a matching sidecar that records `expectedVersion`
 * (with or without a leading `v`). Stat-only; see {@link checkBinaryIdentity}.
 */
export function isTrustedCachedBinary(binaryPath: string, expectedVersion: string): boolean {
  const check = checkBinaryIdentity(binaryPath);
  return check.status === "trusted" && bareVersion(check.version) === bareVersion(expectedVersion);
}

function writeSidecarAtomically(binaryPath: string, identity: BinaryIdentity): void {
  const sidecar = identitySidecarPath(binaryPath);
  const tmp = `${sidecar}.${process.pid}.${Date.now()}.${Math.random().toString(16).slice(2)}.tmp`;
  try {
    writeFileSync(tmp, `${JSON.stringify(identity)}\n`, "utf8");
    if (process.platform === "win32") {
      // renameSync cannot replace an existing file on Windows.
      try {
        unlinkSync(sidecar);
      } catch {
        // Absent is fine; a real failure surfaces from renameSync below.
      }
    }
    renameSync(tmp, sidecar);
  } catch (err) {
    try {
      unlinkSync(tmp);
    } catch {
      // The temp file may never have been created.
    }
    throw err;
  }
}

/**
 * Record the identity of a binary that is already in its final place, with a
 * SHA-256 the caller has already computed (for example while verifying a
 * download against the release checksum). Writes the sidecar atomically
 * (temp file + rename). Synchronous and cheap: one stat and one small write.
 */
export function writeBinaryIdentitySidecar(
  binaryPath: string,
  version: string,
  sha256: string,
): BinaryIdentity {
  const identity: BinaryIdentity = {
    schema: SIDECAR_SCHEMA,
    version: bareVersion(version),
    ...stampOf(binaryPath),
    sha256,
  };
  writeSidecarAtomically(binaryPath, identity);
  return identity;
}

/** Remove the sidecar of `binaryPath`, if any, before that binary is replaced. */
export function removeBinaryIdentitySidecar(binaryPath: string): void {
  try {
    unlinkSync(identitySidecarPath(binaryPath));
  } catch {
    // Absent sidecars are the common case.
  }
}

/**
 * SHA-256 of a file, streamed. The reads run on libuv's thread pool and the
 * digest is fed in small chunks, so the event loop keeps turning while an
 * 85 MB binary is hashed.
 */
export function sha256File(path: string): Promise<string> {
  return new Promise((resolve, reject) => {
    const hash = createHash("sha256");
    const stream = createReadStream(path);
    stream.on("data", (chunk) => hash.update(chunk));
    stream.on("error", reject);
    stream.on("end", () => resolve(hash.digest("hex")));
  });
}

const pendingIdentityWrites = new Set<Promise<boolean>>();

/**
 * Hash `binaryPath` without blocking the event loop and write its sidecar.
 * The file is stat-ed before and after hashing; if it changed in between, no
 * sidecar is written (the hash would describe a file that no longer exists).
 * Never throws; resolves to whether a sidecar was written.
 */
export function recordBinaryIdentity(binaryPath: string, version: string): Promise<boolean> {
  const task = (async () => {
    try {
      const before = stampOf(binaryPath);
      const sha256 = await sha256File(binaryPath);
      const after = stampOf(binaryPath);
      if (!sameStamp(before, after)) {
        warn(`Binary at ${binaryPath} changed while it was being hashed; identity not recorded`);
        return false;
      }
      writeSidecarAtomically(binaryPath, {
        schema: SIDECAR_SCHEMA,
        version: bareVersion(version),
        ...after,
        sha256,
      });
      return true;
    } catch (err) {
      warn(
        `Could not record identity for ${binaryPath}: ${err instanceof Error ? err.message : String(err)}`,
      );
      return false;
    }
  })();
  pendingIdentityWrites.add(task);
  void task.finally(() => pendingIdentityWrites.delete(task));
  return task;
}

/** Test helper: wait for every identity write started by {@link recordBinaryIdentity}. */
export async function __waitForIdentityWritesForTests(): Promise<void> {
  while (pendingIdentityWrites.size > 0) {
    await Promise.all([...pendingIdentityWrites]);
  }
}

/**
 * A key for the file's current size, mtime and inode, or null when the file
 * cannot be stat-ed. Two equal keys mean the file has not been replaced or
 * rewritten in between (as far as stat can tell).
 */
export function binaryStampKey(binaryPath: string): string | null {
  try {
    const stamp = stampOf(binaryPath);
    return `${stamp.size}:${stamp.mtimeNs}:${stamp.ino ?? "-"}`;
  } catch {
    return null;
  }
}

/** Content hashes already computed, keyed by path, valid while the stamp key is unchanged. */
const contentHashCache = new Map<string, { stampKey: string; sha256: string }>();
/** Content hashes being computed, keyed by path and stamp key. */
const contentHashInFlight = new Map<string, Promise<string | null>>();

/**
 * SHA-256 of `binaryPath`'s bytes, computed by streaming the file so the event
 * loop keeps running. Resolves to null when the file is unreadable, or when it
 * changes while being hashed. With `expectedStampKey`, also resolves to null
 * unless the file still has that stamp, so the hash can only describe that
 * exact file. Results are cached until the file's stamp changes.
 */
export function binaryContentHash(
  binaryPath: string,
  expectedStampKey?: string,
): Promise<string | null> {
  const stampKey = binaryStampKey(binaryPath);
  if (stampKey === null) return Promise.resolve(null);
  if (expectedStampKey !== undefined && stampKey !== expectedStampKey) {
    return Promise.resolve(null);
  }
  const cached = contentHashCache.get(binaryPath);
  if (cached && cached.stampKey === stampKey) return Promise.resolve(cached.sha256);
  const flightKey = `${binaryPath}\0${stampKey}`;
  const existing = contentHashInFlight.get(flightKey);
  if (existing) return existing;
  const task = (async () => {
    try {
      const sha256 = await sha256File(binaryPath);
      if (binaryStampKey(binaryPath) !== stampKey) return null;
      contentHashCache.set(binaryPath, { stampKey, sha256 });
      return sha256;
    } catch {
      return null;
    } finally {
      contentHashInFlight.delete(flightKey);
    }
  })();
  contentHashInFlight.set(flightKey, task);
  return task;
}

/**
 * The content hash of `binaryPath` if it is known without reading the file,
 * else null. Known means: a matching identity sidecar (stat check only) or a
 * hash computed earlier for the file's current stamp. On a miss, a streamed
 * hash starts in the background so a later call can answer. Never reads or
 * hashes the file synchronously, so it is safe on a host's main thread.
 */
export function peekBinaryContentHash(binaryPath: string): string | null {
  if (checkBinaryIdentity(binaryPath).status === "trusted") {
    const sha256 = readBinaryIdentity(binaryPath)?.sha256;
    if (sha256) return sha256;
  }
  const stampKey = binaryStampKey(binaryPath);
  if (stampKey === null) return null;
  const cached = contentHashCache.get(binaryPath);
  if (cached && cached.stampKey === stampKey) return cached.sha256;
  void binaryContentHash(binaryPath);
  return null;
}
