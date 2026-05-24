/// <reference path="../bun-test.d.ts" />

/**
 * Tests for the `$HOME` project root behavior in `BridgePool.getBridge`.
 *
 * Historical context (note #65 / commit dc81fc8): when OpenCode Desktop / Pi
 * launches from `~` and a session has no stored project directory, the
 * resolver could hand the plugin the home dir as the "project root".
 * Configuring an aft bridge against `$HOME` walks the entire user home tree
 * (often hundreds of thousands of files), and was a real source of
 * wasted-startup-time complaints.
 *
 * **Current design (Option B — auto-degraded mode):** legitimate migration
 * tasks need to operate from `$HOME` (shell config sweeps, dotfile
 * maintenance), so the pool no longer refuses to spawn. Instead:
 *
 *   1. Plugin eager-configure callers STILL skip eager warmup on `$HOME` via
 *      `isHomeDirectoryRoot()` — Desktop launches from `~` shouldn't auto-warm
 *      a bridge no one asked for.
 *   2. `BridgePool.getBridge()` accepts `$HOME` and spawns a bridge normally.
 *   3. The Rust `handle_configure` detects `canonical_root == $HOME` and
 *      auto-disables heavy subsystems (`search_index`, `semantic_search`),
 *      then records `degraded_reasons: ["home_root"]` on the status snapshot.
 *      Sidebar + `/aft-status` surface the degraded state.
 *
 * These tests pin the pool-side behavior. Rust-side degraded-mode behavior is
 * covered separately in `crates/aft/tests/integration/configure_test.rs`.
 */

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { BridgePool, HomeProjectRootError, isHomeDirectoryRoot } from "../pool.js";

describe("isHomeDirectoryRoot", () => {
  test("returns true for the user's home directory", () => {
    expect(isHomeDirectoryRoot(homedir())).toBe(true);
  });

  test("returns false for a subdirectory of $HOME", () => {
    // A real subdir of $HOME — guaranteed to exist if $HOME exists, but we
    // don't actually need it to exist for the path-comparison check.
    const sub = join(homedir(), "some-project");
    expect(isHomeDirectoryRoot(sub)).toBe(false);
  });

  test("returns false for an unrelated absolute path", () => {
    expect(isHomeDirectoryRoot("/usr/local/bin")).toBe(false);
  });

  test("returns false for empty string", () => {
    expect(isHomeDirectoryRoot("")).toBe(false);
  });

  test("returns false for a tempdir", () => {
    expect(isHomeDirectoryRoot(tmpdir())).toBe(false);
  });
});

describe("BridgePool.getBridge — $HOME spawn behavior", () => {
  test("spawns a bridge for $HOME (no refusal)", () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity });
    // Migration tasks legitimately need to operate from $HOME. The bridge is
    // constructed lazily (no real subprocess until .send() is called), so we
    // can verify the public contract with a fake binary path.
    const bridge = pool.getBridge(homedir());
    expect(bridge).toBeDefined();
    expect(pool.size).toBe(1);
  });

  test("returns the same bridge instance on repeated $HOME calls", () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity });
    const b1 = pool.getBridge(homedir());
    const b2 = pool.getBridge(homedir());
    // Pool keys by canonical project root; second call must hit the cache.
    expect(b2).toBe(b1);
    expect(pool.size).toBe(1);
  });

  test("spawns a bridge for a subdirectory of $HOME", () => {
    const sub = mkdtempSync(join(homedir(), ".aft-pool-home-guard-test-"));
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity });
    const bridge = pool.getBridge(sub);
    expect(bridge).toBeDefined();
  });

  test("spawns a bridge for tempdir (the common test fixture)", () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity });
    const dir = tmpdir();
    expect(() => pool.getBridge(dir)).not.toThrow();
  });
});

describe("HomeProjectRootError (legacy export)", () => {
  test("is still exported for backwards-compatible imports", () => {
    // The error class is preserved so existing imports don't break, but
    // the pool no longer throws it. New code should not check for it.
    const err = new HomeProjectRootError("/fake/home");
    expect(err.name).toBe("HomeProjectRootError");
    expect(err.projectRoot).toBe("/fake/home");
    expect(err.message).toContain("user home directory");
  });
});

// Hooks: keep the test file lifecycle quiet — no real spawns, no real
// network. Guard against accidental side-effects.
beforeAll(() => {});
afterAll(() => {});
