/// <reference path="../../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { linkSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { _precheckArchiveSizeForTesting as precheckArchiveContents } from "../../lsp-github-install.js";

function hasBsdtar(): boolean {
  try {
    const command = process.platform === "win32" ? "tar.exe" : "tar";
    return execFileSync(command, ["--version"], { encoding: "utf8" }).includes("bsdtar");
  } catch {
    return false;
  }
}

function writeStoredZip(path: string, entryName: string, content = "payload\n"): void {
  const name = Buffer.from(entryName);
  const body = Buffer.from(content);
  const local = Buffer.alloc(30 + name.length);
  local.writeUInt32LE(0x04034b50, 0);
  local.writeUInt16LE(20, 4);
  local.writeUInt32LE(body.length, 18);
  local.writeUInt32LE(body.length, 22);
  local.writeUInt16LE(name.length, 26);
  name.copy(local, 30);

  const central = Buffer.alloc(46 + name.length);
  central.writeUInt32LE(0x02014b50, 0);
  central.writeUInt16LE(20, 4);
  central.writeUInt16LE(20, 6);
  central.writeUInt32LE(body.length, 20);
  central.writeUInt32LE(body.length, 24);
  central.writeUInt16LE(name.length, 28);
  name.copy(central, 46);

  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(1, 8);
  end.writeUInt16LE(1, 10);
  end.writeUInt32LE(central.length, 12);
  end.writeUInt32LE(local.length + body.length, 16);
  writeFileSync(path, Buffer.concat([local, body, central, end]));
}

describe("github LSP archive security", () => {
  test("lists a real zip with the host platform tool", () => {
    const root = mkdtempSync(join(tmpdir(), "aft-zip-list-"));
    try {
      const archive = join(root, "payload.zip");
      writeStoredZip(archive, "bin/server");

      expect(() => precheckArchiveContents(archive, "zip")).not.toThrow();
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test.skipIf(!hasBsdtar())("lists a real zip through the Windows tar-compatible path", () => {
    const root = mkdtempSync(join(tmpdir(), "aft-zip-tar-list-"));
    try {
      const archive = join(root, "payload.zip");
      writeStoredZip(archive, "bin/server");

      expect(() => precheckArchiveContents(archive, "zip", "tar")).not.toThrow();
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test.skipIf(!hasBsdtar())("rejects zip traversal entries on the tar listing path", () => {
    const root = mkdtempSync(join(tmpdir(), "aft-zip-slip-list-"));
    try {
      const archive = join(root, "payload.zip");
      writeStoredZip(archive, "../escape.txt");

      expect(() => precheckArchiveContents(archive, "zip", "tar")).toThrow(/escapes archive root/i);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test.skipIf(process.platform === "win32")(
    "rejects tar hardlink entries before extraction",
    () => {
      const root = mkdtempSync(join(tmpdir(), "aft-hardlink-"));
      try {
        const src = join(root, "src");
        const archive = join(root, "payload.tar.gz");
        execFileSync("mkdir", ["-p", src]);
        writeFileSync(join(src, "target"), "payload\n");
        linkSync(join(src, "target"), join(src, "hardlink"));
        execFileSync("tar", ["-czf", archive, "-C", src, "."]);

        expect(() => precheckArchiveContents(archive, "tar.gz")).toThrow(/hardlink/i);
      } finally {
        rmSync(root, { recursive: true, force: true });
      }
    },
  );
});
