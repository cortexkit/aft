/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseAftVersionOutput, readBinaryVersionOffThread } from "../version-probe.js";
import { writeAftFixture, writeAftVersionFixture } from "./test-utils/aft-executable-fixture.js";

describe("readBinaryVersionOffThread", () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "aft-offthread-probe-"));
  });

  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  test("reads the version of a real binary", async () => {
    const binary = writeAftVersionFixture(join(dir, "aft"), "3.4.5");
    await expect(readBinaryVersionOffThread(binary)).resolves.toBe("3.4.5");
  });

  test("resolves null for a missing binary", async () => {
    await expect(readBinaryVersionOffThread(join(dir, "missing"))).resolves.toBeNull();
  });

  test("the event loop keeps running while a slow binary is probed", async () => {
    // A binary that takes 600 ms to answer stands in for a cold binary being
    // paged in. A probe on the calling thread would stop every timer until it
    // returned; on a worker the interval keeps firing throughout.
    const binary = writeAftFixture(join(dir, "slow-aft"), {
      stdout: "aft 1.0.0\n",
      sleepMs: 600,
    });
    let ticks = 0;
    const interval = setInterval(() => {
      ticks += 1;
    }, 20);
    try {
      const version = await readBinaryVersionOffThread(binary);
      expect(version).toBe("1.0.0");
    } finally {
      clearInterval(interval);
    }
    expect(ticks).toBeGreaterThanOrEqual(10);
  });
});

describe("parseAftVersionOutput", () => {
  test("strips the aft prefix and prefers stdout", () => {
    expect(parseAftVersionOutput("aft 0.9.0\n", "aft 0.1.0")).toBe("0.9.0");
    expect(parseAftVersionOutput("", "aft 0.74.0\n")).toBe("0.74.0");
    expect(parseAftVersionOutput("  ", "")).toBeNull();
  });
});
