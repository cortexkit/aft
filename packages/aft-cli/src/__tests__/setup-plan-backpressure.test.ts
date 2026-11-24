/// <reference path="../bun-test.d.ts" />

import { expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { copyFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { linkCachedExecutable } from "../../../aft-bridge/src/__tests__/test-utils/cached-executable.js";
import { getAftBinaryName } from "../lib/paths.js";
import { getSelfVersion } from "../lib/self-version.js";

const CLI_ENTRY = fileURLToPath(new URL("../index.ts", import.meta.url));
const PACKAGE_JSON = fileURLToPath(new URL("../../package.json", import.meta.url));

test("setup --plan drains a backpressured stdout pipe before exiting", async () => {
  const root = mkdtempSync(join(tmpdir(), "aft-cli-plan-backpressure-"));
  try {
    const build = await Bun.build({
      entrypoints: [CLI_ENTRY],
      outdir: join(root, "dist"),
      target: "node",
      format: "esm",
    });
    if (!build.success) throw new Error(build.logs.join("\n"));
    copyFileSync(PACKAGE_JSON, join(root, "package.json"));

    const plan = JSON.stringify({
      plan_version: 1,
      features: [],
      padding: "x".repeat(900_000),
    });
    const planPath = join(root, "plan.json");
    writeFileSync(planPath, plan);
    const cacheRoot = join(root, "cache");
    const binaryDir = join(cacheRoot, "bin", `v${getSelfVersion()}`);
    mkdirSync(binaryDir, { recursive: true });
    linkCachedExecutable(join(binaryDir, getAftBinaryName()), '#!/bin/sh\ncat "$AFT_TEST_PLAN"\n');

    const home = join(root, "home");
    mkdirSync(home, { recursive: true });
    const child = spawn(
      process.env.NODE ?? "node",
      [join(root, "dist", "index.js"), "setup", "--plan"],
      {
        env: {
          AFT_CACHE_DIR: cacheRoot,
          AFT_TEST_PLAN: planPath,
          HOME: home,
          PATH: process.env.PATH ?? "/usr/bin:/bin",
          XDG_CACHE_HOME: join(root, "xdg-cache"),
          XDG_CONFIG_HOME: join(root, "xdg-config"),
          XDG_DATA_HOME: join(root, "xdg-data"),
        },
        stdio: ["ignore", "pipe", "pipe"],
      },
    );
    const closed = once(child, "close");
    let stderr = "";
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk: string) => {
      stderr += chunk;
    });
    child.stdout.pause();
    await new Promise((resolve) => setTimeout(resolve, 250));

    let stdout = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => {
      stdout += chunk;
    });
    child.stdout.resume();
    const [code] = (await closed) as [number | null];

    expect(code).toBe(0);
    expect(stderr).toBe("");
    expect(stdout).toBe(plan);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}, 20_000);
