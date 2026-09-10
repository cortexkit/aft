/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import {
  __shutdownCleanupInvocationCountForTests,
  registerShutdownCleanup,
  runCleanups,
} from "../shutdown-hooks.js";

type RegisteredCleanup = (reason: string) => Promise<void>;

// Drain the globalThis guard between tests so we can simulate independent loads.
function resetShutdownHookState(): void {
  const g = globalThis as unknown as Record<string, unknown>;
  delete g.__aftShutdownHooks__;
}

function registeredCleanups(): Set<RegisteredCleanup> {
  const state = (
    globalThis as unknown as {
      __aftShutdownHooks__?: { cleanups: Set<RegisteredCleanup> };
    }
  ).__aftShutdownHooks__;
  if (!state) throw new Error("shutdown-hook state was not initialized");
  return state.cleanups;
}

describe("registerShutdownCleanup", () => {
  afterEach(() => {
    resetShutdownHookState();
  });

  test("registers and unregisters a cleanup without error", () => {
    const registration = registerShutdownCleanup(() => {});
    expect(typeof registration.dispose).toBe("function");
    expect(typeof registration.unregister).toBe("function");
    registration.unregister();
  });

  test("stores multiple cleanups per Node process", () => {
    registerShutdownCleanup(() => {});
    registerShutdownCleanup(() => {});
    expect(registeredCleanups().size).toBe(2);
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

    // Clean up tracked callbacks so this test does not affect a later drain.
    registeredCleanups().clear();
  });

  test("runCleanups executes registered cleanups with the exit reason and allows later runs", async () => {
    const calls: string[] = [];
    registerShutdownCleanup((reason) => {
      calls.push(`first:${reason}`);
    });

    await runCleanups("SIGTERM");

    registerShutdownCleanup((reason) => {
      calls.push(`second:${reason}`);
    });
    await runCleanups("beforeExit");

    expect(calls).toEqual(["first:SIGTERM", "second:beforeExit"]);
  });

  test("unregister prevents the cleanup from being tracked", () => {
    const registration = registerShutdownCleanup(() => {});
    expect(registeredCleanups().size).toBe(1);

    registration.unregister();
    expect(registeredCleanups().size).toBe(0);
  });

  test("individual dispose removes its cleanup before the process-exit drain", async () => {
    const calls: string[] = [];
    const registration = registerShutdownCleanup((reason) => {
      calls.push(`owned:${reason}`);
    });
    const ownedCleanup = [...registeredCleanups()][0];
    registerShutdownCleanup((reason) => {
      calls.push(`sibling:${reason}`);
    });

    await registration.dispose("dispose");
    await runCleanups("beforeExit");

    expect(calls).toEqual(["owned:dispose", "sibling:beforeExit"]);
    expect(__shutdownCleanupInvocationCountForTests(ownedCleanup)).toBe(1);
    expect(registeredCleanups().has(ownedCleanup)).toBe(false);
  });

  test("individual dispose is idempotent", async () => {
    let resourceCleanupCalls = 0;
    const registration = registerShutdownCleanup(() => {
      resourceCleanupCalls += 1;
    });
    const ownedCleanup = [...registeredCleanups()][0];

    await registration.dispose("dispose");
    await registration.dispose("dispose");

    expect(resourceCleanupCalls).toBe(1);
    expect(__shutdownCleanupInvocationCountForTests(ownedCleanup)).toBe(1);
  });
});
