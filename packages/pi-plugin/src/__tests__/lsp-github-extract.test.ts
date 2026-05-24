/**
 * Tests for `validateExtraction` in lsp-github-install.ts.
 *
 * Audit v0.17 #2: total uncompressed bytes capped at MAX_EXTRACT_BYTES (1 GiB).
 * Mirrors the OpenCode plugin tests.
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
  const root = mkdtempSync(join(tmpdir(), "aft-pi-extract-tests-"));
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

describe("validateExtraction (Pi)", () => {
  test("accepts a normal extraction", () => {
    const staging = createStagingFixture();
    mkdirSync(join(staging, "bin"), { recursive: true });
    writeFileSync(join(staging, "bin", "lsp-binary"), "binary");
    expect(() => validateExtraction(staging)).not.toThrow();
  });

  test("rejects symlinks (zip-slip defense)", () => {
    const staging = createStagingFixture();
    writeFileSync(join(staging, "real.txt"), "real");
    symlinkSync("real.txt", join(staging, "link.txt"));
    expect(() => validateExtraction(staging)).toThrow(/symlink.*zip-slip defense/);
  });

  test("rejects sparse file > 1 GiB cap (decompression bomb)", async () => {
    const staging = createStagingFixture();
    const fs = await import("node:fs");
    const fh = fs.openSync(join(staging, "sparse.bin"), "w");
    try {
      fs.ftruncateSync(fh, 1024 * 1024 * 1024 + 1);
    } finally {
      fs.closeSync(fh);
    }
    expect(() => validateExtraction(staging)).toThrow(/decompression bomb defense/);
  });

  test("rejects accumulated bytes across files exceeding cap", async () => {
    const staging = createStagingFixture();
    const fs = await import("node:fs");
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
});
