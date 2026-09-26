/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { linkCachedExecutable } from "../../../aft-bridge/src/__tests__/test-utils/cached-executable.js";
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
      linkCachedExecutable(binaryPath, '#!/bin/sh\nprintf "aft 9.8.7\\n"\n');

      expect(probeBinaryVersion("9.8.7")).toBe("9.8.7");
    });
  });
});
