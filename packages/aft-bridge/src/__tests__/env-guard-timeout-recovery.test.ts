/// <reference path="../bun-test.d.ts" />

import { beforeEach, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { acquireEnv, withEnv } from "./test-utils/env-guard.js";

const fixtureMode = "AFT_ENV_GUARD_RECOVERY_FIXTURE";
const existingKey = "AFT_ENV_GUARD_EXISTING";
const absentKey = "AFT_ENV_GUARD_ABSENT";

if (process.env[fixtureMode] === "1") {
  describe.serial("environment lifecycle fixture", () => {
    let resume!: () => void;
    let timedOutBody!: Promise<void>;

    test("fixture: withEnv times out holding the lock", () => {
      timedOutBody = withEnv({ [existingKey]: "timed out", [absentKey]: "temporary" }, async () => {
        expect(process.env[existingKey]).toBe("timed out");
        await new Promise<void>((resolve) => {
          resume = resolve;
        });
      });
      return timedOutBody;
    }, 100);

    test("fixture: timeout restores env and late finally preserves the next owner", async () => {
      expect(process.env[existingKey]).toBe("original");
      expect(process.env[absentKey]).toBeUndefined();
      let queued!: Promise<() => void>;
      let entered = false;
      await withEnv({ [existingKey]: "next owner", [absentKey]: "next value" }, async () => {
        resume();
        await timedOutBody;
        expect(process.env[existingKey]).toBe("next owner");
        expect(process.env[absentKey]).toBe("next value");
        queued = acquireEnv({ [existingKey]: "queued owner" }).then((release) => {
          entered = true;
          return release;
        });
        // A stale release must not let a waiter enter while the next owner is active.
        await new Promise((resolve) => setTimeout(resolve, 10));
        expect(entered).toBe(false);
      });
      const release = await queued;
      expect(entered).toBe(true);
      expect(process.env[existingKey]).toBe("queued owner");
      release();
      expect(process.env[existingKey]).toBe("original");
      expect(process.env[absentKey]).toBeUndefined();
    }, 500);

    let staleRelease!: () => void;
    describe("acquireEnv in beforeEach", () => {
      beforeEach(async () => {
        staleRelease = await acquireEnv({ [existingKey]: "hook owner" });
      }, 500);

      test("fixture: acquireEnv times out holding the lock", async () => {
        expect(process.env[existingKey]).toBe("hook owner");
        await new Promise(() => {});
      }, 100);
    });

    test("fixture: acquireEnv timeout restores env and stale callback is harmless", async () => {
      expect(process.env[existingKey]).toBe("original");
      await withEnv({ [existingKey]: "later owner" }, () => {
        staleRelease();
        expect(process.env[existingKey]).toBe("later owner");
      });
    }, 500);

    test("fixture: passing test leaves an unreleased acquisition", async () => {
      await acquireEnv({ [existingKey]: "passing owner" });
      expect(process.env[existingKey]).toBe("passing owner");
    }, 500);

    test("fixture: failing test leaves an unreleased acquisition", async () => {
      expect(process.env[existingKey]).toBe("original");
      await acquireEnv({ [existingKey]: "failing owner" });
      throw new Error("intentional env fixture failure");
    }, 500);

    test("fixture: failure restores env and releases the lock", async () => {
      expect(process.env[existingKey]).toBe("original");
      await withEnv({ [existingKey]: "final owner" }, () => {
        expect(process.env[existingKey]).toBe("final owner");
      });
    }, 500);
  });
} else {
  test("environment fixtures recover after timeout, pass, failure, and late cleanup", () => {
    const env: NodeJS.ProcessEnv = {
      ...process.env,
      [fixtureMode]: "1",
      [existingKey]: "original",
    };
    delete env[absentKey];
    // Keep intentional test failures in a child so the regular suite stays green.
    const result = spawnSync(process.execPath, ["test", import.meta.path], {
      env,
      encoding: "utf8",
      timeout: 10_000,
    });
    const output = `${result.stdout}\n${result.stderr}`;
    expect(result.error, output).toBeUndefined();
    expect(result.status, output).toBe(1);
    expect(output).toContain("4 pass");
    expect(output).toContain("3 fail");
    expect(output).toContain("intentional env fixture failure");
    expect(output).toContain(
      "(pass) environment lifecycle fixture > fixture: timeout restores env",
    );
    expect(output).toContain(
      "(pass) environment lifecycle fixture > fixture: acquireEnv timeout restores env",
    );
    expect(output).toContain(
      "(pass) environment lifecycle fixture > fixture: failure restores env",
    );
  }, 15_000);
}
