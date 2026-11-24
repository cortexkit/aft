import { expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, rmSync, statSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { getActiveLogger, setActiveLogger } from "../active-logger.js";
import {
  __fileDigestWorkForTests,
  cachedFileSha256,
  readStampedFileDigest,
  writeStampedFileDigest,
} from "../binary-identity.js";
import { __test__ } from "../onnx-runtime.js";

test("ONNX stamped startups hash zero bytes and reject a changed library", async () => {
  const root = mkdtempSync(join(tmpdir(), "aft-onnx-stamp-"));
  const previousLogger = getActiveLogger();
  const errors: string[] = [];
  setActiveLogger({ log: () => {}, warn: () => {}, error: (line) => errors.push(line) });
  try {
    const dir = join(root, "onnxruntime", __test__.ORT_VERSION);
    mkdirSync(dir, { recursive: true });
    const library = join(dir, "libonnxruntime.dylib");
    const digest = createHash("sha256").update("library!").digest("hex");
    writeFileSync(library, "library!");
    writeFileSync(
      join(dir, __test__.ONNX_INSTALLED_META_FILE),
      JSON.stringify({ version: __test__.ORT_VERSION, sha256: digest }),
    );
    writeStampedFileDigest(library, digest);
    const before = __fileDigestWorkForTests();
    for (let i = 0; i < 10; i++) {
      expect(
        await __test__.resolveOnnxRuntime(root, { platformInfo: null, systemSearchPaths: [] }),
      ).toBe(dir);
    }
    expect(__fileDigestWorkForTests().asyncHashes - before.asyncHashes).toBe(0);
    expect(__fileDigestWorkForTests().bytesHashed - before.bytesHashed).toBe(0);
    const old = statSync(library);
    writeFileSync(library, "tampered");
    utimesSync(library, old.atime, old.mtime);
    expect(
      await __test__.resolveOnnxRuntime(root, { platformInfo: null, systemSearchPaths: [] }),
    ).toBeNull();
    expect(__fileDigestWorkForTests().asyncHashes - before.asyncHashes).toBe(1);
    expect(errors).toHaveLength(1);
    expect(errors[0]).toContain("npx @cortexkit/aft doctor --fix");
  } finally {
    setActiveLogger(previousLogger!);
    rmSync(root, { recursive: true, force: true });
  }
});

test("legacy digests stream once and persist across later resolutions", async () => {
  const root = mkdtempSync(join(tmpdir(), "aft-digest-legacy-"));
  try {
    const file = join(root, "library");
    writeFileSync(file, "legacy");
    const before = __fileDigestWorkForTests();
    expect(await cachedFileSha256(file)).toBe(createHash("sha256").update("legacy").digest("hex"));
    expect(await cachedFileSha256(file)).toBe(createHash("sha256").update("legacy").digest("hex"));
    expect(__fileDigestWorkForTests().asyncHashes - before.asyncHashes).toBe(1);
    expect(__fileDigestWorkForTests().bytesHashed - before.bytesHashed).toBe(6);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("a digest sidecar cannot stamp a replacement with an earlier file's hash", async () => {
  const root = mkdtempSync(join(tmpdir(), "aft-digest-race-"));
  try {
    const file = join(root, "library");
    writeFileSync(file, "before");
    const s = statSync(file, { bigint: true });
    const verifiedStamp = `${s.dev}:${s.ino}:${s.size}:${s.mtimeNs}:${s.ctimeNs}`;
    const digest = createHash("sha256").update("before").digest("hex");
    writeFileSync(file, "after!");
    writeStampedFileDigest(file, digest, verifiedStamp);
    expect(readStampedFileDigest(file)).toBeNull();
    expect(await cachedFileSha256(file)).toBe(createHash("sha256").update("after!").digest("hex"));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
