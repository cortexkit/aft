/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { registerShutdownCleanup } from "../shutdown-hooks.js";

// Drain the globalThis guard between tests so we can simulate independent loads.
function resetShutdownHookState(): void {
  const g = globalThis as unknown as Record<string, unknown>;
  delete g.__aftShutdownHooks__;
}

describe("registerShutdownCleanup", () => {
  afterEach(() => {
    resetShutdownHookState();
  });

  test("registers and unregisters a cleanup without error", () => {
    const unregister = registerShutdownCleanup(() => {});
    expect(typeof unregister).toBe("function");
    unregister(); // should not throw
  });

  test("stores multiple cleanups per Node process", () => {
    const callOrder: number[] = [];
    registerShutdownCleanup(() => {
      callOrder.push(1);
    });
    registerShutdownCleanup(() => {
      callOrder.push(2);
    });
    // Reach into the global state to verify both landed in the same Set.
    const state = (globalThis as unknown as Record<string, { cleanups: Set<unknown> }>)
      .__aftShutdownHooks__;
    expect(state).toBeDefined();
    expect(state?.cleanups.size).toBe(2);
  });

  test("installs a single set of OS-level listeners even across reloads", () => {
    // Initial installation adds SIGTERM/SIGINT/SIGHUP listeners once.
    const before = process.listenerCount("SIGTERM");
    registerShutdownCleanup(() => {});
    const after1 = process.listenerCount("SIGTERM");
    expect(after1).toBe(before + 1);

    // Re-registering another cleanup must NOT add another process-level listener.
    registerShutdownCleanup(() => {});
    const after2 = process.listenerCount("SIGTERM");
    expect(after2).toBe(after1);

    // Clean up the listener we attached so test runner isn't polluted.
    const state = (
      globalThis as unknown as {
        __aftShutdownHooks__?: { cleanups: Set<unknown>; installed: boolean };
      }
    ).__aftShutdownHooks__;
    if (state) state.cleanups.clear();
  });

  test("unregister prevents the cleanup from being tracked", () => {
    const fn = () => {};
    const unregister = registerShutdownCleanup(fn);
    const state = (globalThis as unknown as { __aftShutdownHooks__?: { cleanups: Set<unknown> } })
      .__aftShutdownHooks__;
    expect(state?.cleanups.has(fn)).toBe(true);

    unregister();
    expect(state?.cleanups.has(fn)).toBe(false);
  });
});
