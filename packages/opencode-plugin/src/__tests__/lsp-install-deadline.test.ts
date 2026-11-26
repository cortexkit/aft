import { describe, expect, test } from "bun:test";
import type { ChildProcess } from "node:child_process";
import { EventEmitter } from "node:events";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { PassThrough } from "node:stream";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { _runInstallForTesting as runInstall } from "../lsp-auto-install.js";
import { NPM_LSP_TABLE } from "../lsp-npm-table.js";

/** Run `fn` with a private AFT cache and a fake `npm` first on PATH. */
async function cacheFixture(fn: (root: string) => Promise<void> | void): Promise<void> {
  const root = mkdtempSync(join(tmpdir(), "aft-install-deadline-"));
  const bin = join(root, "bin");
  mkdirSync(bin);
  // Model npm's package identity; spawning is replaced so the test never runs
  // a system installer.
  writeFileSync(
    join(bin, "package.json"),
    JSON.stringify({ name: "npm", version: "11.6.0", bin: { npm: "npm" } }),
  );
  writeFileSync(join(bin, "npm"), "#!/bin/sh\nexit 0\n", { mode: 0o755 });
  try {
    await withEnv(
      { AFT_CACHE_DIR: join(root, "cache"), PATH: `${bin}${delimiter}${process.env.PATH ?? ""}` },
      () => fn(root),
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

function settlesWithin<T>(promise: Promise<T>, ms: number): Promise<boolean> {
  return Promise.race([
    promise.then(() => true),
    new Promise<boolean>((resolve) => setTimeout(() => resolve(false), ms)),
  ]);
}

// Pi has the same deadline; without it a stalled npm holds the install lock
// every OpenCode and Pi process shares.
describe("npm deadline", () => {
  test("stalled npm settles only after deadline termination is confirmed", async () => {
    await cacheFixture(async (root) => {
      const spec = NPM_LSP_TABLE[0];
      if (!spec) throw new Error("npm fixture missing");
      const controller = new AbortController();
      const retained: boolean[] = [];
      // A child that never exits: only the deadline can end this install.
      const child = Object.assign(new EventEmitter(), {
        pid: 123,
        exitCode: null,
        signalCode: null,
        stdout: new PassThrough(),
        stderr: new PassThrough(),
        unref: () => {},
      }) as unknown as ChildProcess;
      let confirmTermination = () => {};
      let onTerminating = () => {};
      const terminating = new Promise<void>((resolve) => {
        onTerminating = resolve;
      });
      const runtime = {
        spawn: (() => child) as unknown as typeof import("@cortexkit/aft-bridge").spawn,
        terminate: (() => {
          onTerminating();
          return new Promise<void>((resolve) => {
            confirmTermination = resolve;
          });
        }) as typeof import("@cortexkit/aft-bridge").terminateNpmProcessTree,
      };
      // Bounds the test if the deadline never fires.
      const rescue = setTimeout(() => controller.abort(), 2_000);
      try {
        let settled = false;
        const installing = runInstall(
          spec,
          "1.0.0",
          root,
          { retain: () => retained.push(true), release: () => {} },
          undefined,
          controller.signal,
          0,
          runtime,
        );
        void installing.then(() => {
          settled = true;
        });
        expect(await settlesWithin(terminating, 1_000)).toBe(true);
        expect(settled).toBe(false);
        confirmTermination();
        expect(await installing).toBe(false);
        expect(controller.signal.aborted).toBe(false);
        expect(retained).toEqual([]);
      } finally {
        confirmTermination();
        clearTimeout(rescue);
      }
    });
  });
});
