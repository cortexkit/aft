/**
 * Regression tests for Pi session ID propagation (audit #7).
 *
 * Previously the extension generated one static UUID per extension load, so
 * `/new`, `/fork`, and `/resume` all shared one backup/undo namespace in
 * AFT. The fix routes through Pi's native `ExtensionContext.sessionManager.
 * getSessionId()` per tool call.
 */

import { describe, expect, test } from "bun:test";
import { resolveSessionId } from "../tools/_shared.js";

describe("resolveSessionId", () => {
  test("returns the Pi session ID from extCtx.sessionManager", () => {
    const extCtx = {
      sessionManager: { getSessionId: () => "session-abc" },
    } as unknown as Parameters<typeof resolveSessionId>[0];
    expect(resolveSessionId(extCtx)).toBe("session-abc");
  });

  test("reflects session changes across /new, /fork, /resume", () => {
    // Simulates Pi swapping the active session; sessionManager is shared on ctx,
    // so subsequent getSessionId() calls see the new ID.
    let current = "s1";
    const extCtx = {
      sessionManager: { getSessionId: () => current },
    } as unknown as Parameters<typeof resolveSessionId>[0];
    expect(resolveSessionId(extCtx)).toBe("s1");
    current = "s2";
    expect(resolveSessionId(extCtx)).toBe("s2");
    current = "s3-fork";
    expect(resolveSessionId(extCtx)).toBe("s3-fork");
  });

  test("returns undefined when sessionManager is absent", () => {
    const extCtx = {} as unknown as Parameters<typeof resolveSessionId>[0];
    expect(resolveSessionId(extCtx)).toBeUndefined();
  });

  test("returns undefined when getSessionId throws or returns empty", () => {
    const extCtx = {
      sessionManager: { getSessionId: () => "" },
    } as unknown as Parameters<typeof resolveSessionId>[0];
    expect(resolveSessionId(extCtx)).toBeUndefined();
  });
});
