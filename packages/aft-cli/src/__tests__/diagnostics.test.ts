/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { appendFileSync, mkdtempSync, truncateSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { tailLogFile } from "../lib/diagnostics.js";

describe("tailLogFile", () => {
  test("tails a large log from the end", () => {
    const dir = mkdtempSync(join(tmpdir(), "aft-cli-tail-test-"));
    const path = join(dir, "large.log");
    writeFileSync(path, "start\n");
    truncateSync(path, 100 * 1024 * 1024);
    appendFileSync(path, "line-1\nline-2\nline-3\n");

    expect(tailLogFile(path, 2)).toBe("line-2\nline-3");
  });
});
