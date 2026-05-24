/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { homedir, userInfo } from "node:os";
import { sanitizeContent, sanitizeValue } from "../lib/sanitize.js";

describe("sanitizeContent", () => {
  const originalHome = homedir();
  const originalUser = userInfo().username;

  afterEach(() => {
    // These tests never mutate env/os, but keep the pattern in case future
    // tests need it.
  });

  test("replaces home directory with ~", () => {
    const input = `Error at ${originalHome}/foo/bar`;
    const out = sanitizeContent(input);
    expect(out).not.toContain(originalHome);
    expect(out).toContain("~/foo/bar");
  });

  test("replaces macOS /Users/<name>/ with <USER>", () => {
    const input = "Reading /Users/alice/.config/opencode/aft.jsonc";
    const out = sanitizeContent(input);
    expect(out).not.toContain("/Users/alice");
    expect(out).toContain("/Users/<USER>");
  });

  test("replaces Linux /home/<name>/ with <USER>", () => {
    const input = "Reading /home/bob/.config/opencode/aft.jsonc";
    const out = sanitizeContent(input);
    expect(out).not.toContain("/home/bob");
    expect(out).toContain("/home/<USER>");
  });

  test("replaces standalone username occurrences", () => {
    // Only meaningful when the test runner actually has a username.
    if (!originalUser) return;
    const input = `Config for ${originalUser} loaded`;
    const out = sanitizeContent(input);
    expect(out).not.toContain(originalUser);
    expect(out).toContain("<USER>");
  });

  test("is idempotent", () => {
    const input = `at ${originalHome}/foo`;
    const once = sanitizeContent(input);
    const twice = sanitizeContent(once);
    expect(twice).toBe(once);
  });

  test("sanitizes issue-title-sized strings", () => {
    const input = `AFT issue: failure under ${originalHome}/secret-project`;
    const out = sanitizeContent(input);
    expect(out).not.toContain(originalHome);
    expect(out).toContain("~/secret-project");
  });
});

describe("sanitizeValue", () => {
  test("walks nested objects and arrays", () => {
    const input = {
      logs: [`line1 ${homedir()}/x`, `line2 ${homedir()}/y`],
      nested: {
        path: `${homedir()}/config/file.jsonc`,
        keep: 42,
      },
    };
    const out = sanitizeValue(input) as typeof input;
    expect(out.logs[0]).not.toContain(homedir());
    expect(out.logs[0]).toContain("~/x");
    expect(out.nested.path).not.toContain(homedir());
    expect(out.nested.keep).toBe(42);
  });

  test("preserves primitives", () => {
    expect(sanitizeValue(null)).toBeNull();
    expect(sanitizeValue(undefined)).toBeUndefined();
    expect(sanitizeValue(123)).toBe(123);
    expect(sanitizeValue(true)).toBe(true);
  });
});
