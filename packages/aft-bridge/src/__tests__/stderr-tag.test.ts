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
import { tagStderrLine } from "../bridge.js";

describe("tagStderrLine — never doubles the [aft] prefix", () => {
  test("line already tagged with [aft] is left as-is (no doubling)", () => {
    const line = "[aft] semantic index: rebuilding from scratch";
    expect(tagStderrLine(line)).toBe(line);
  });

  test("line already tagged with [aft-lsp] is left as-is (preserves LSP tag)", () => {
    const line = "[aft-lsp] [ses_abc123] failed to spawn TypeScript Language Server";
    expect(tagStderrLine(line)).toBe(line);
  });

  test("line tagged with [aft-bridge] (forward-compat with future tags) is preserved", () => {
    // `[aft-<word>]` family. We don't currently emit `[aft-bridge]` from Rust
    // but the regex must not collapse hypothetical future tags into `[aft]`.
    const line = "[aft-bridge] something happened";
    expect(tagStderrLine(line)).toBe(line);
  });

  test("untagged line gets [aft] prepended (rare child-library output)", () => {
    const line = "stack overflow at 0xdeadbeef";
    expect(tagStderrLine(line)).toBe("[aft] stack overflow at 0xdeadbeef");
  });

  test("line containing [aft] mid-string but starting differently is treated as untagged", () => {
    // The tag check is anchored to start-of-line. A panic message that
    // mentions `[aft]` later in the text must still get the outer `[aft]`
    // prefix, otherwise it'd render as a bare unowned message.
    const line = "panicked at 'unwrap on None'; see [aft] log for details";
    expect(tagStderrLine(line)).toBe(`[aft] ${line}`);
  });

  test("line with numeric subtag like [aft1] is NOT recognized as a tag", () => {
    // The regex requires `[aft-<word>]` or exactly `[aft]`. A weird literal
    // `[aft1]` is treated as untagged so we still own the outer prefix.
    const line = "[aft1] weird";
    expect(tagStderrLine(line)).toBe(`[aft] ${line}`);
  });

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
