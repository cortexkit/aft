/// <reference path="../bun-test.d.ts" />

/**
 * Regression tests for stderr line tagging in `BinaryBridge`.
 *
 * The bug we're guarding against: env_logger in the `aft` Rust child emits
 * each log line with an outer `[aft]` or `[aft-lsp]` tag based on log target.
 * Before the v0.19.1 fix, the bridge then prepended ANOTHER `[aft]` to every
 * line because the strip regex only matched `^[aft] ` and missed `[aft-lsp]`.
 * Combined with the plugin logger's `[aft-plugin]` outer wrap, LSP errors
 * rendered as `[aft-plugin] [aft] [aft-lsp] [aft] [ses_xxx] ...`.
 *
 * `tagStderrLine` is the pure helper that decides whether to prepend `[aft]`.
 * These tests pin its contract.
 */

import { describe, expect, test } from "bun:test";
import { BinaryBridge, shouldSurfaceStderrLine, tagStderrLine } from "../bridge.js";

describe("engine stderr flush severity", () => {
  function capture() {
    const calls: [string, string][] = [];
    const bridge = new BinaryBridge("/unused/aft", process.cwd(), {
      logger: {
        log: (message) => calls.push(["INFO", message]),
        debug: (message) => calls.push(["DEBUG", message]),
        warn: (message) => calls.push(["WARN", message]),
        error: (message) => calls.push(["ERROR", message]),
      },
    });
    return { calls, bridge: bridge as any };
  }

  for (const level of ["INFO", "WARN", "ERROR", "DEBUG"]) {
    test(`routes ${level} to its sink without changing the message`, () => {
      const { calls, bridge } = capture();
      const line = `2026-10-23T21:26:36.123Z [aft-lsp] ${level} payload  with spaces Ω`;
      bridge.onStderrData(`${line}\n`);
      expect(calls).toEqual([[level, `[aft] ${line}`]]);
      expect(bridge.formatStderrTail()).toContain(`[aft] ${line}`);
    });
  }

  test("flushes an unterminated record and leaves unrecognized stderr on INFO", () => {
    const { calls, bridge } = capture();
    const lines = [
      "2026-10-23T21:26:36.123Z [aft] WARN pending record",
      "noise 2026-10-23T21:26:36.123Z [aft] ERROR not an engine prefix",
      "2026-10-23T21:26:36.123Z [third-party] WARN not an engine tag",
      "[aft] WARN legacy unstructured text",
    ];
    for (const line of lines) {
      bridge.onStderrData(line);
      bridge.flushStderrBuffer();
    }
    expect(calls).toEqual(
      lines.map((line, index) => [index === 0 ? "WARN" : "INFO", tagStderrLine(line)]),
    );
  });
});

describe("tagStderrLine — never doubles the [aft] prefix", () => {
  for (const { name, line, expected } of [
    {
      name: "line already tagged with [aft] is left as-is (no doubling)",
      line: "[aft] semantic index: rebuilding from scratch",
      expected: "[aft] semantic index: rebuilding from scratch",
    },
    {
      name: "line already tagged with [aft-lsp] is left as-is (preserves LSP tag)",
      line: "[aft-lsp] [ses_abc123] failed to spawn TypeScript Language Server",
      expected: "[aft-lsp] [ses_abc123] failed to spawn TypeScript Language Server",
    },
    {
      name: "line tagged with [aft-bridge] (forward-compat with future tags) is preserved",
      line: "[aft-bridge] something happened",
      expected: "[aft-bridge] something happened",
    },
    {
      name: "untagged line gets [aft] prepended (rare child-library output)",
      line: "stack overflow at 0xdeadbeef",
      expected: "[aft] stack overflow at 0xdeadbeef",
    },
    {
      name: "line containing [aft] mid-string but starting differently is treated as untagged",
      line: "panicked at 'unwrap on None'; see [aft] log for details",
      expected: "[aft] panicked at 'unwrap on None'; see [aft] log for details",
    },
    {
      name: "line with numeric subtag like [aft1] is NOT recognized as a tag",
      line: "[aft1] weird",
      expected: "[aft] [aft1] weird",
    },
  ]) {
    test(name, () => {
      expect(tagStderrLine(line)).toBe(expected);
    });
  }

  test("no double-tag regression for the canonical LSP spawn-failure shape", () => {
    // Reproduces the exact line shape that produced
    // `[aft-plugin] [aft] [aft-lsp] [aft] ...` before the fix.
    const line =
      "[aft-lsp] [ses_313660571ffeZTsf4koSJwk50Q] failed to spawn TypeScript Language Server: server error -32603: Could not find a valid TypeScript installation";
    const tagged = tagStderrLine(line);
    // Must NOT start with `[aft] [aft-lsp]` — that's the bug.
    expect(tagged.startsWith("[aft] [aft-lsp]")).toBe(false);
    // Must preserve the original tag.
    expect(tagged.startsWith("[aft-lsp]")).toBe(true);
  });

  test("empty string would not normally be passed in but is handled safely", () => {
    // Defense-in-depth: the production caller filters empty lines before
    // calling tagStderrLine. If something slips through, we still produce
    // a deterministic non-doubled output.
    expect(tagStderrLine("")).toBe("[aft] ");
  });

  test("line with leading whitespace is treated as untagged (we don't trim)", () => {
    // env_logger never emits leading whitespace. If something else does,
    // we don't pretend the tag matches; we add our own.
    const line = "  [aft] indented";
    expect(tagStderrLine(line)).toBe(`[aft] ${line}`);
  });
});

describe("shouldSurfaceStderrLine — filters only benign cpuinfo sandbox noise", () => {
  test("filters the known ONNX/cpuinfo /proc/cpuinfo parse failure", () => {
    expect(
      shouldSurfaceStderrLine(
        "Error in cpuinfo: failed to parse processor information from /proc/cpuinfo",
      ),
    ).toBe(false);
  });

  test("keeps genuine stderr errors visible", () => {
    expect(shouldSurfaceStderrLine("Error in aft: failed to parse request JSON")).toBe(true);
    expect(shouldSurfaceStderrLine("Error in cpuinfo: failed to allocate processor table")).toBe(
      true,
    );
  });
});
