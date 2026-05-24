/// <reference path="../../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { linkSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { _precheckArchiveSizeForTesting as precheckArchiveContents } from "../../lsp-github-install.js";

describe("github LSP archive security", () => {
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
