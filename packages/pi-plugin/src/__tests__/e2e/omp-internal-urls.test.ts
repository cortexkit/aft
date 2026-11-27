/// <reference path="../../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { stagedE2eBinary } from "../../../../../tests/helpers/e2e-binary.js";

const host = process.env.AFT_OMP_HOST;
if (!host)
  console.warn(
    "[omp-internal-urls] skipped: set AFT_OMP_HOST to a directory with OMP 18.6.x installed",
  );

async function runHeadless(extensionEntry?: string): Promise<void> {
  const home = mkdtempSync(join(tmpdir(), "aft-omp-urls-"));
  try {
    mkdirSync(join(home, "project"));
    const env = {
      ...process.env,
      HOME: home,
      XDG_CONFIG_HOME: join(home, "config"),
      XDG_DATA_HOME: join(home, "data"),
      XDG_CACHE_HOME: join(home, "cache"),
      XDG_STATE_HOME: join(home, "state"),
      XDG_RUNTIME_DIR: join(home, "run"),
      AFT_STORAGE_DIR: join(home, "data", "cortexkit", "aft"),
      AFT_CACHE_DIR: join(home, "cache", "aft"),
      AFT_OMP_HOST: resolve(host!),
      // Use the build under test, not first-run downloads into this empty HOME.
      // Delegated startup keeps the installers lazy until an AFT filesystem call.
      AFT_BINARY_PATH:
        stagedE2eBinary(process.env) ??
        resolve(
          import.meta.dir,
          `../../../../../target/debug/aft${process.platform === "win32" ? ".exe" : ""}`,
        ),
      MAGIC_CONTEXT_PI_SUBAGENT: "1",
      AFT_LOG_STDERR: "1",
      ...(extensionEntry ? { AFT_OMP_EXTENSION_ENTRY: extensionEntry } : {}),
    };
    const child = Bun.spawn(
      [process.execPath, resolve(import.meta.dir, "../fixtures/omp-internal-urls-harness.ts")],
      { env, cwd: join(home, "project"), stdout: "pipe", stderr: "pipe", timeout: 120_000 },
    );
    const [exit, stdout, stderr] = await Promise.all([
      child.exited,
      new Response(child.stdout).text(),
      new Response(child.stderr).text(),
    ]);
    expect(exit, `${stdout}\n${stderr}`).toBe(0);
    const evidence = JSON.parse(stdout.trim().split("\n").at(-1)!);
    expect(evidence.version).toMatch(/^OMP 18\.6\./);
    expect(evidence.checks).toBe(5);
    expect(evidence.registeredMcp).toBe(true);
    expect(evidence.skillPrompt).toBe(true);
    expect(evidence.filesystemDiagnostics.write).toContain("content");
    expect(evidence.filesystemDiagnostics.edit).toContain("old_string");
    expect(evidence.filesystemDiagnostics.grep).toContain("case");
    expect(evidence.filesystemDiagnostics.bash).toContain("cwd");
  } finally {
    rmSync(home, { recursive: true, force: true });
  }
}

describe.skipIf(!host)("OMP 18.6.x headless internal URLs", () => {
  test("AFT tool surface preserves agent reads, content-less proc kill and skills in an isolated HOME", async () => {
    await runHeadless();
  }, 150_000);

  test("AFT bundled entry preserves agent reads, content-less proc kill and skills in an isolated HOME", async () => {
    await runHeadless(
      process.env.AFT_OMP_EXTENSION_ENTRY ?? resolve(import.meta.dir, "../../../dist/index.js"),
    );
  }, 150_000);
});
