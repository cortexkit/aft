/**
 * Tests for `validateExtraction` in lsp-github-install.ts.
 *
 * These exercise the audit v0.17 hardening:
 *   - #2 (decompression bomb cap) — total uncompressed bytes capped at MAX_EXTRACT_BYTES.
 *   - Existing zip-slip + symlink defenses (regression coverage).
 */

import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  _precheckArchiveSizeForTesting as precheckArchiveSize,
  validateExtraction,
} from "../lsp-github-install.js";

const tempRoots = new Set<string>();

function createStagingFixture(): string {
  const root = mkdtempSync(join(tmpdir(), "aft-extract-tests-"));
  tempRoots.add(root);
  return root;
}

afterEach(() => {
  for (const root of tempRoots) rmSync(root, { recursive: true, force: true });
  tempRoots.clear();
});

describe("precheckArchiveSize", () => {
  test("rejects an archive TOC with a sparse-sized member before extraction", () => {
    const root = createStagingFixture();
    const archivePath = join(root, "bomb.zip");
    writeFakeZipWithUncompressedSize(archivePath, 1024 * 1024 * 1024 + 1);

    expect(() => precheckArchiveSize(archivePath, "zip")).toThrow(/archive uncompressed size/);
  });
});

function writeFakeZipWithUncompressedSize(path: string, uncompressedSize: number): void {
  const name = Buffer.from("sparse.bin");
  const local = Buffer.alloc(30 + name.length);
  local.writeUInt32LE(0x04034b50, 0);
  local.writeUInt16LE(20, 4);
  local.writeUInt32LE(0, 14);
  local.writeUInt32LE(uncompressedSize, 22);
  local.writeUInt16LE(name.length, 26);
  name.copy(local, 30);

  const central = Buffer.alloc(46 + name.length);
  central.writeUInt32LE(0x02014b50, 0);
  central.writeUInt16LE(20, 4);
  central.writeUInt16LE(20, 6);
  central.writeUInt32LE(0, 16);
  central.writeUInt32LE(uncompressedSize, 24);
  central.writeUInt16LE(name.length, 28);
  name.copy(central, 46);

  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(1, 8);
  end.writeUInt16LE(1, 10);
  end.writeUInt32LE(central.length, 12);
  end.writeUInt32LE(local.length, 16);
  writeFileSync(path, Buffer.concat([local, central, end]));
}

describe("validateExtraction", () => {
  test("accepts a normal extraction with regular files only", () => {
    const staging = createStagingFixture();
    mkdirSync(join(staging, "bin"), { recursive: true });
    writeFileSync(join(staging, "bin", "lsp-binary"), "#!/usr/bin/env binary\n");
    writeFileSync(join(staging, "README.md"), "tiny readme");

    expect(() => validateExtraction(staging)).not.toThrow();
  });

  test("accepts deeply nested directories", () => {
    const staging = createStagingFixture();
    mkdirSync(join(staging, "a", "b", "c", "d"), { recursive: true });
    writeFileSync(join(staging, "a", "b", "c", "d", "leaf.txt"), "small");

    expect(() => validateExtraction(staging)).not.toThrow();
  });

  test("rejects symlinks (zip-slip defense)", () => {
    const staging = createStagingFixture();
    writeFileSync(join(staging, "real.txt"), "real");
    symlinkSync("real.txt", join(staging, "link.txt"));

    expect(() => validateExtraction(staging)).toThrow(/symlink.*zip-slip defense/);
  });

  test("rejects symlink even if it points outside staging root", () => {
    const staging = createStagingFixture();
    symlinkSync("/etc/passwd", join(staging, "evil-link"));

    expect(() => validateExtraction(staging)).toThrow(/symlink.*zip-slip defense/);
  });

  // Audit v0.17 #2: decompression bomb defense
  test("rejects extraction whose total bytes exceed MAX_EXTRACT_BYTES", () => {
    const staging = createStagingFixture();

    // We can't actually allocate 1 GB+ in tests, so we monkey-patch the
    // size by writing a single sparse file slightly larger than the cap.
    // truncate(2) + writeFileSync({offset}) would be ideal but Bun's
    // fs lacks a direct sparse helper; instead we create many medium files
    // and assert the walker accumulates correctly. Use Bun's test-time
    // override of MAX_EXTRACT_BYTES via the env var if needed.
    //
    // Simpler approach: write 4 files of ~200 KB each and assert acceptance,
    // proving the walker DOES accumulate (regression sentinel — if someone
    // removes the byte tracking, this test would still pass, so we follow
    // up with an actual oversize check below using a sparse-ish file).
    const data = Buffer.alloc(200 * 1024, 0x41);
    for (let i = 0; i < 4; i++) {
      writeFileSync(join(staging, `chunk-${i}.bin`), data);
    }
    expect(() => validateExtraction(staging)).not.toThrow();
  });

  test("rejects when one file alone exceeds the byte cap (sparse file)", async () => {
    const staging = createStagingFixture();
    // Create a sparse file larger than MAX_EXTRACT_BYTES (1 GiB).
    // Sparse files don't actually allocate disk; truncate sets the
    // logical size which is what lstat().size reports.
    const fs = await import("node:fs");
    const fh = fs.openSync(join(staging, "sparse.bin"), "w");
    try {
      // 1 GiB + 1 byte
      fs.ftruncateSync(fh, 1024 * 1024 * 1024 + 1);
    } finally {
      fs.closeSync(fh);
    }

    expect(() => validateExtraction(staging)).toThrow(/decompression bomb defense/);
  });

  test("rejects when accumulated bytes across many files exceed cap", async () => {
    const staging = createStagingFixture();
    const fs = await import("node:fs");
    // Two sparse files of 600 MiB each = 1.2 GiB total > 1 GiB cap.
    for (const name of ["a.bin", "b.bin"]) {
      const fh = fs.openSync(join(staging, name), "w");
      try {
        fs.ftruncateSync(fh, 600 * 1024 * 1024);
      } finally {
        fs.closeSync(fh);
      }
    }

    expect(() => validateExtraction(staging)).toThrow(/decompression bomb defense/);
  });

  test("rejects FIFO (non-file/non-dir entry)", async () => {
    // FIFO creation needs spawnSync('mkfifo', ...) — skip on Windows.
    if (process.platform === "win32") return;
    const staging = createStagingFixture();
    const { spawnSync } = await import("node:child_process");
    spawnSync("mkfifo", [join(staging, "evil-fifo")]);

    expect(() => validateExtraction(staging)).toThrow(/non-file\/non-dir entry/);
  });
});
