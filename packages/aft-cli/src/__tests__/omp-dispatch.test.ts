/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";

import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { getInstalledAdapters } from "../adapters/index.js";
import { resolveAdaptersForCommand } from "../lib/harness-select.js";

let root: string;
let releaseEnv: (() => void) | undefined;
const originalPath = process.env.PATH ?? "";

beforeEach(async () => {
  root = mkdtempSync(join(tmpdir(), "aft-cli-omp-dispatch-"));
  const binDir = join(root, "bin");
  mkdirSync(binDir, { recursive: true });
  if (process.platform === "win32") {
    writeFileSync(join(binDir, "omp.cmd"), "@echo off\r\nexit /b 0\r\n");
  } else {
    const omp = join(binDir, "omp");
    writeFileSync(omp, "#!/bin/sh\nexit 0\n");
    chmodSync(omp, 0o755);
  }

  releaseEnv = await acquireEnv({
    HOME: join(root, "home"),
    USERPROFILE: join(root, "home"),
    XDG_CONFIG_HOME: undefined,
    XDG_DATA_HOME: undefined,
    OPENCODE_CONFIG_DIR: join(root, "missing-opencode"),
    PI_CONFIG_DIR: undefined,
    PI_CODING_AGENT_DIR: undefined,
    PI_PACKAGE_DIR: undefined,
    PI_PROFILE: undefined,
    PI_CONFIG_FILES: undefined,
    OMP_PROFILE: undefined,
    PATH: `${binDir}${delimiter}${originalPath}`,
  });
});

afterEach(() => {
  releaseEnv?.();
  releaseEnv = undefined;
  rmSync(root, { recursive: true, force: true });
});

describe("OMP harness dispatch", () => {
  test("an installed omp binary joins automatic harness detection", () => {
    expect(getInstalledAdapters().map((adapter) => adapter.kind)).toContain("omp");
  });

  test("--harness omp selects only OmpAdapter", async () => {
    const adapters = await resolveAdaptersForCommand(["--harness", "omp"], {
      allowMulti: true,
      verb: "setup",
    });

    expect(adapters).toHaveLength(1);
    expect(adapters[0]?.kind).toBe("omp");
  });
});
