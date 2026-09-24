/// <reference path="../bun-test.d.ts" />

/**
 * Identity sidecars let a plugin host trust a cached `aft` binary with a stat
 * instead of executing it. These tests pin what the sidecar records, that each
 * recorded field is actually compared, and that it is written atomically.
 */
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import {
  appendFileSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  __waitForIdentityWritesForTests,
  type BinaryIdentity,
  binaryContentHash,
  binaryStampKey,
  checkBinaryIdentity,
  identitySidecarPath,
  isTrustedCachedBinary,
  peekBinaryContentHash,
  readBinaryIdentity,
  recordBinaryIdentity,
  writeBinaryIdentitySidecar,
} from "../binary-identity.js";

let dir: string;
let binary: string;

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "aft-identity-test-"));
  mkdirSync(join(dir, "v1.2.3"), { recursive: true });
  binary = join(dir, "v1.2.3", "aft");
  writeFileSync(binary, "binary bytes for 1.2.3");
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

function rewriteSidecar(patch: Partial<BinaryIdentity>): void {
  const current = readBinaryIdentity(binary);
  if (!current) throw new Error("sidecar missing");
  writeFileSync(identitySidecarPath(binary), JSON.stringify({ ...current, ...patch }));
}

describe("sidecar trust check", () => {
  test("a matching sidecar is trusted for its recorded version only", () => {
    writeBinaryIdentitySidecar(binary, "v1.2.3", "a".repeat(64));

    expect(checkBinaryIdentity(binary)).toEqual({ status: "trusted", version: "1.2.3" });
    expect(isTrustedCachedBinary(binary, "1.2.3")).toBe(true);
    expect(isTrustedCachedBinary(binary, "v1.2.3")).toBe(true);
    expect(isTrustedCachedBinary(binary, "1.2.4")).toBe(false);
  });

  test("records version, size, mtime, inode and sha256 of the final file", () => {
    const identity = writeBinaryIdentitySidecar(binary, "1.2.3", "b".repeat(64));
    const stat = statSync(binary, { bigint: true });

    expect(identity).toEqual({
      schema: 1,
      version: "1.2.3",
      size: stat.size.toString(),
      mtimeNs: stat.mtimeNs.toString(),
      ino: process.platform === "win32" && stat.ino === 0n ? null : stat.ino.toString(),
      sha256: "b".repeat(64),
    });
    expect(readBinaryIdentity(binary)).toEqual(identity);
  });

  test("a missing sidecar is reported as missing and not trusted", () => {
    expect(checkBinaryIdentity(binary)).toEqual({ status: "missing" });
    expect(isTrustedCachedBinary(binary, "1.2.3")).toBe(false);
  });

  test("a malformed sidecar is treated as missing", () => {
    writeFileSync(identitySidecarPath(binary), "{not json");
    expect(checkBinaryIdentity(binary)).toEqual({ status: "missing" });
  });

  test("a size mismatch rejects", () => {
    writeBinaryIdentitySidecar(binary, "1.2.3", "c".repeat(64));
    rewriteSidecar({ size: "1" });

    const check = checkBinaryIdentity(binary);
    expect(check.status).toBe("mismatch");
    expect(check.status === "mismatch" && check.reason).toContain("size");
    expect(isTrustedCachedBinary(binary, "1.2.3")).toBe(false);
  });

  test("an mtime mismatch rejects", () => {
    writeBinaryIdentitySidecar(binary, "1.2.3", "c".repeat(64));
    rewriteSidecar({ mtimeNs: "1" });

    const check = checkBinaryIdentity(binary);
    expect(check.status).toBe("mismatch");
    expect(check.status === "mismatch" && check.reason).toContain("mtime");
  });

  test("an inode mismatch rejects", () => {
    writeBinaryIdentitySidecar(binary, "1.2.3", "c".repeat(64));
    rewriteSidecar({ ino: "1" });

    const check = checkBinaryIdentity(binary);
    expect(check.status).toBe("mismatch");
    expect(check.status === "mismatch" && check.reason).toContain("inode");
  });

  test("modifying the binary after the sidecar was written rejects", () => {
    writeBinaryIdentitySidecar(binary, "1.2.3", "c".repeat(64));
    appendFileSync(binary, "tampered");

    expect(checkBinaryIdentity(binary).status).toBe("mismatch");
  });

  test("a sidecar whose binary is gone rejects", () => {
    writeBinaryIdentitySidecar(binary, "1.2.3", "c".repeat(64));
    rmSync(binary);

    expect(checkBinaryIdentity(binary).status).toBe("mismatch");
  });
});

describe("sidecar writes", () => {
  test("write is atomic: only the sidecar remains, no temp files", () => {
    writeBinaryIdentitySidecar(binary, "1.2.3", "d".repeat(64));
    writeBinaryIdentitySidecar(binary, "1.2.3", "e".repeat(64));

    expect(readdirSync(join(dir, "v1.2.3")).sort()).toEqual(["aft", "aft.identity.json"]);
    expect(readBinaryIdentity(binary)?.sha256).toBe("e".repeat(64));
  });

  test("recordBinaryIdentity hashes the bytes without the caller supplying them", async () => {
    await expect(recordBinaryIdentity(binary, "1.2.3")).resolves.toBe(true);

    const expected = createHash("sha256").update(readFileSync(binary)).digest("hex");
    expect(readBinaryIdentity(binary)?.sha256).toBe(expected);
    expect(isTrustedCachedBinary(binary, "1.2.3")).toBe(true);
  });

  test("recordBinaryIdentity never writes a sidecar for a missing binary", async () => {
    rmSync(binary);
    await expect(recordBinaryIdentity(binary, "1.2.3")).resolves.toBe(false);
    await __waitForIdentityWritesForTests();
    expect(readdirSync(join(dir, "v1.2.3"))).toEqual([]);
  });
});

describe("content hash for bridge hot-swap detection", () => {
  test("a matching sidecar answers with its recorded sha256 without reading the file", () => {
    // A sha256 that is not the file's real hash proves the answer came from
    // the sidecar rather than from hashing the bytes.
    writeBinaryIdentitySidecar(binary, "1.2.3", "f".repeat(64));
    expect(peekBinaryContentHash(binary)).toBe("f".repeat(64));
  });

  test("without a sidecar the first peek is unknown, and the background hash answers later", async () => {
    const expected = createHash("sha256").update(readFileSync(binary)).digest("hex");

    expect(peekBinaryContentHash(binary)).toBeNull();
    await expect(binaryContentHash(binary)).resolves.toBe(expected);
    expect(peekBinaryContentHash(binary)).toBe(expected);
  });

  test("a changed file invalidates the cached hash", async () => {
    await binaryContentHash(binary);
    rmSync(binary);
    writeFileSync(binary, "different, longer bytes for a rebuilt binary");

    expect(peekBinaryContentHash(binary)).toBeNull();
    await expect(binaryContentHash(binary)).resolves.toBe(
      createHash("sha256").update(readFileSync(binary)).digest("hex"),
    );
  });

  test("a hash tied to a stamp refuses a file that no longer has it", async () => {
    const stampAtSpawn = binaryStampKey(binary) as string;
    rmSync(binary);
    writeFileSync(binary, "replaced after the spawn");

    await expect(binaryContentHash(binary, stampAtSpawn)).resolves.toBeNull();
  });
});
