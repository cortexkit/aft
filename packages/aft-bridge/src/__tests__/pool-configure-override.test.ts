/// <reference path="../bun-test.d.ts" />

/**
 * Tests for `BridgePool.setConfigureOverride`.
 *
 * The override map is consumed at bridge spawn time. The contract:
 *   - Setting an override updates the future-spawn map.
 *   - Setting an override does NOT restart existing bridges (existing bridges
 *     keep their original configure payload — their warm trigram/semantic/LSP/
 *     symbol-cache state must survive an async ONNX download settling).
 *   - Setting an override to `undefined` removes it.
 *   - Constructor configOverrides is preserved as the baseline; setConfigureOverride
 *     layers on top without dropping unrelated keys.
 *
 * We avoid real binary spawns by passing a fake binary path. `getBridge`
 * still constructs a `BinaryBridge`, which spawns lazily on the first
 * `send()` — that's fine for these tests because we're not calling send().
 * The `_testGetConfigOverrides()` getter lets us inspect the future-spawn
 * map directly without round-tripping through the spawn pipeline.
 */

import { describe, expect, test } from "bun:test";
import { BridgePool } from "../pool.js";

describe("BridgePool.setConfigureOverride", () => {
  test("setting an override updates the future-spawn map", () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity });

    expect(pool._testGetConfigOverrides()).toEqual({});

    pool.setConfigureOverride("_ort_dylib_dir", "/onnx/runtime");

    expect(pool._testGetConfigOverrides()).toEqual({
      _ort_dylib_dir: "/onnx/runtime",
    });
  });

  test("setting an override does NOT restart existing bridges (size stays the same)", () => {
    // The contract: existing bridges keep their original configure payload.
    // We can't observe a "restart" without spawning real binaries, but we
    // can observe that the pool's bridge count does NOT change when an
    // override is set after a bridge already exists. If `setConfigureOverride`
    // ever started restarting bridges (the regression we're guarding against),
    // the pool would either grow or shrink during this call.
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity });

    pool.getBridge("/project/a");
    expect(pool.size).toBe(1);

    // The override change must NOT trigger any bridge lifecycle work.
    pool.setConfigureOverride("_ort_dylib_dir", "/onnx/late");
    expect(pool.size).toBe(1);

    // Asking for the same project root returns the existing bridge.
    pool.getBridge("/project/a");
    expect(pool.size).toBe(1);
  });

  test("subsequent overrides for the same key replace, don't accumulate", () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity });

    pool.setConfigureOverride("_ort_dylib_dir", "/first");
    pool.setConfigureOverride("_ort_dylib_dir", "/second");

    expect(pool._testGetConfigOverrides()).toEqual({
      _ort_dylib_dir: "/second",
    });
  });

  test("setting override to `undefined` removes it", () => {
    const pool = new BridgePool("/fake/aft", { idleTimeoutMs: Infinity });

    pool.setConfigureOverride("_ort_dylib_dir", "/onnx/early");
    expect(pool._testGetConfigOverrides()._ort_dylib_dir).toBe("/onnx/early");

    pool.setConfigureOverride("_ort_dylib_dir", undefined);
    expect(pool._testGetConfigOverrides()._ort_dylib_dir).toBeUndefined();
    // The key itself should be deleted, not just set to undefined.
    expect("_ort_dylib_dir" in pool._testGetConfigOverrides()).toBe(false);
  });

  test("constructor configOverrides is the baseline; setConfigureOverride mutates it", () => {
    const pool = new BridgePool(
      "/fake/aft",
      { idleTimeoutMs: Infinity },
      { restrict_to_project_root: true, baseline_key: "kept" },
    );

    pool.setConfigureOverride("_ort_dylib_dir", "/onnx/path");

    expect(pool._testGetConfigOverrides()).toEqual({
      restrict_to_project_root: true,
      baseline_key: "kept",
      _ort_dylib_dir: "/onnx/path",
    });
  });

  test("removing one key preserves other baseline keys", () => {
    const pool = new BridgePool(
      "/fake/aft",
      { idleTimeoutMs: Infinity },
      { restrict_to_project_root: true, _ort_dylib_dir: "/initial" },
    );

    pool.setConfigureOverride("_ort_dylib_dir", undefined);

    expect(pool._testGetConfigOverrides()).toEqual({
      restrict_to_project_root: true,
    });
  });
});
