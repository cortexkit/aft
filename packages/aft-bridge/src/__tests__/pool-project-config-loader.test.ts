/// <reference path="../bun-test.d.ts" />

/**
 * Tests for `BridgePool.projectConfigLoader` — the per-project override
 * loader introduced for OpenCode Desktop / `opencode serve` mode.
 *
 * The contract:
 *   - The loader is invoked exactly once per new bridge, with the canonical
 *     (normalized) project root.
 *   - Reusing an existing bridge does NOT re-invoke the loader (cached).
 *   - The loader is NOT invoked for `getActiveBridgeForRoot`, which is the
 *     read-only path used by `/aft-status` polling.
 *   - A loader throw is swallowed; the bridge spawns with global overrides
 *     only and the error is logged via the pool logger.
 *   - Each new project root gets its own loader call.
 *
 * We don't have a public API to inspect a bridge's merged configure payload
 * without spawning the real binary, so we assert behavior through loader
 * call observation: the loader is THE merge surface, so the merge correctness
 * is enforced by `pool.ts` directly. What we MUST guard against is the loader
 * being called the wrong number of times or with the wrong root.
 */

import { describe, expect, test } from "bun:test";
import { BridgePool } from "../pool.js";

describe("BridgePool.projectConfigLoader", () => {
  test("loader invoked exactly once per new bridge", () => {
    const calls: string[] = [];
    const pool = new BridgePool("/fake/aft", {
      idleTimeoutMs: Infinity,
      projectConfigLoader: (root) => {
        calls.push(root);
        return {};
      },
    });

    pool.getBridge("/project/a");
    expect(calls).toEqual(["/project/a"]);
  });

  test("reusing the same project root does NOT re-invoke the loader", () => {
    // The contract: existing bridges keep the overrides they were spawned
    // with. Re-invoking the loader on every getBridge() call would create a
    // perf trap (config file I/O per tool call) AND change the spawn's
    // override map on a path that pretends to be a cache hit.
    const calls: string[] = [];
    const pool = new BridgePool("/fake/aft", {
      idleTimeoutMs: Infinity,
      projectConfigLoader: (root) => {
        calls.push(root);
        return {};
      },
    });

    pool.getBridge("/project/a");
    pool.getBridge("/project/a");
    pool.getBridge("/project/a");
    expect(calls).toEqual(["/project/a"]);
  });

  test("each new project root triggers its own loader call", () => {
    // OpenCode Desktop / opencode serve: different sessions in different
    // project subdirectories. The loader is the per-project override seam,
    // so it must fire ONCE per distinct canonical project root.
    const calls: string[] = [];
    const pool = new BridgePool("/fake/aft", {
      idleTimeoutMs: Infinity,
      projectConfigLoader: (root) => {
        calls.push(root);
        return {};
      },
    });

    pool.getBridge("/project/a");
    pool.getBridge("/project/b");
    pool.getBridge("/project/c");

    expect(calls).toEqual(["/project/a", "/project/b", "/project/c"]);
  });

  test("getActiveBridgeForRoot does NOT invoke the loader", () => {
    // The active-bridge path is read-only (status polling, bg-completion
    // drains) — it must NEVER create a new bridge or invoke configure logic.
    const calls: string[] = [];
    const pool = new BridgePool("/fake/aft", {
      idleTimeoutMs: Infinity,
      projectConfigLoader: (root) => {
        calls.push(root);
        return {};
      },
    });

    const bridge = pool.getActiveBridgeForRoot("/project/never-spawned");
    expect(bridge).toBeNull();
    expect(calls).toEqual([]);
  });

  test("loader throw is swallowed; bridge still spawns", () => {
    // The contract: loader failure must NOT block bridge creation. If the
    // user's per-project aft.jsonc is malformed, the bridge falls back to
    // the global overrides — the agent can still read/write/edit/bash even
    // if its per-project experimental flags didn't load.
    let throwCount = 0;
    const pool = new BridgePool("/fake/aft", {
      idleTimeoutMs: Infinity,
      projectConfigLoader: () => {
        throwCount++;
        throw new Error("malformed aft.jsonc");
      },
    });

    // Should not throw.
    const bridge = pool.getBridge("/project/a");
    expect(bridge).toBeDefined();
    expect(throwCount).toBe(1);
    expect(pool.size).toBe(1);
  });

  test("loader receives a normalized (path-stripped) project root", () => {
    // Trailing separators must be stripped before the loader runs so callers
    // can use the returned string as a cache key without extra normalization.
    const calls: string[] = [];
    const pool = new BridgePool("/fake/aft", {
      idleTimeoutMs: Infinity,
      projectConfigLoader: (root) => {
        calls.push(root);
        return {};
      },
    });

    pool.getBridge("/project/a/");
    pool.getBridge("/project/a"); // same canonical key — should NOT re-invoke

    expect(calls.length).toBe(1);
    expect(calls[0]?.endsWith("/")).toBe(false);
  });

  test("pool works without a loader (back-compat)", () => {
    // Existing pool users (Pi today, all callers before this release) must
    // keep working when they don't supply a loader.
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity });
    const bridge = pool.getBridge("/project/a");
    expect(bridge).toBeDefined();
    expect(pool.size).toBe(1);
  });
});
