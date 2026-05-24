/**
 * Unit tests for the brace-aware include-arg splitter used by Pi's hoisted
 * grep tool. See packages/opencode-plugin/src/__tests__/search.test.ts for
 * the OpenCode side of the same fix; both plugins must split identically.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { splitIncludeGlobs } from "../tools/hoisted.js";

describe("splitIncludeGlobs", () => {
  test("splits plain comma-separated patterns", () => {
    expect(splitIncludeGlobs("*.ts,*.tsx")).toEqual(["*.ts", "*.tsx"]);
  });

  test("preserves a single brace group as one pattern (regression)", () => {
    // Naive split-by-`,` chops "**/*.{vue,ts}" into "**/*.{vue" + "ts}",
    // yielding the user-facing "unclosed alternate group; missing '}'"
    // globset error.
    expect(splitIncludeGlobs("**/*.{vue,ts,tsx}")).toEqual(["**/*.{vue,ts,tsx}"]);
  });

  test("splits top-level commas while preserving nested brace groups", () => {
    expect(splitIncludeGlobs("*.ts,**/*.{vue,tsx},*.go")).toEqual([
      "*.ts",
      "**/*.{vue,tsx}",
      "*.go",
    ]);
  });

  test("handles nested braces correctly", () => {
    expect(splitIncludeGlobs("**/*.{a,{b,c},d}")).toEqual(["**/*.{a,{b,c},d}"]);
  });

  test("trims whitespace and drops empty segments", () => {
    expect(splitIncludeGlobs(" *.ts , *.tsx , ")).toEqual(["*.ts", "*.tsx"]);
  });

  test("tolerates an unclosed brace by treating remaining commas as content (no crash)", () => {
    expect(splitIncludeGlobs("**/*.{vue,ts")).toEqual(["**/*.{vue,ts"]);
  });
});
