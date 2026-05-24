/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { probeBinaryVersion } from "../lib/binary-probe.js";
import { getAftBinaryName } from "../lib/paths.js";

describe("probeBinaryVersion", () => {
  test("uses spawn argv against the binary resolved by findAftBinary", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-cli-binary-probe-test-"));
    await withEnv({ AFT_CACHE_DIR: root }, () => {
      const binDir = join(root, "bin", "v9.8.7");
      mkdirSync(binDir, { recursive: true });
      const binaryPath = join(binDir, getAftBinaryName());
      writeFileSync(binaryPath, '#!/bin/sh\nprintf "aft 9.8.7\\n"\n');
      chmodSync(binaryPath, 0o755);

      expect(probeBinaryVersion("9.8.7")).toBe("9.8.7");
    });
  });
});
