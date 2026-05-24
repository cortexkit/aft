/// <reference path="../bun-test.d.ts" />
/**
 * Regression coverage for the `ctx.ask` Promise contract.
 *
 * The SDK contract for `ask()` has flipped twice:
 *   - pre-1.14:           Promise<void>
 *   - 1.14.x – 1.15.4:    Effect.Effect<void>   (silent-await bug landed here)
 *   - 1.15.5+:            Promise<void>         (current AFT target)
 *
 * These tests pin the rules that hold under EITHER shape, restated for the
 * Promise contract we ship against today:
 *
 *   - rules MUST actually execute (no silent drop of the awaited body)
 *   - allow resolves cleanly (askEditPermission returns undefined)
 *   - deny surfaces as a rejected Promise with the underlying Error.message
 *     intact, so askEditPermission's try/catch can pass it through to the
 *     user-facing denial response.
 *
 * The file name is kept (`permissions-effect.test.ts`) to preserve git history;
 * the contents target the Promise shape that `@opencode-ai/plugin@1.15.5`
 * declares for `ToolContext["ask"]`.
 */
import { describe, expect, test } from "bun:test";
import type { ToolContext } from "@opencode-ai/plugin";
import {
  askEditPermission,
  askGlobPermission,
  askGrepPermission,
  runAsk,
} from "../tools/permissions.js";

describe("runAsk + Promise", () => {
  test("a resolving Promise body actually runs through runAsk (allow path)", async () => {
    let executed = false;
    const ask = (async () => {
      executed = true;
    })();
    await runAsk(ask);
    // Regression sentinel: if runAsk regressed to a no-op or fire-and-forget,
    // we'd silently drop the ask body and the user's policy would never run.
    expect(executed).toBe(true);
  });

  test("a rejecting Promise surfaces the underlying Error (deny path)", async () => {
    const denied = Promise.reject(new Error("Permission denied: bash deny rule"));
    await expect(runAsk(denied)).rejects.toThrow("Permission denied: bash deny rule");
  });

  test("askEditPermission returns undefined when ask resolves", async () => {
    const ctx = makeMockContext(async () => {});
    const result = await askEditPermission(ctx, ["src/foo.ts"]);
    // Convention: undefined = allowed; a string = denial reason.
    expect(result).toBeUndefined();
  });

  test("askEditPermission reports unsupported host when context.ask is missing", async () => {
    const ctx = {
      ...makeMockContext(async () => {}),
      ask: undefined,
    } as unknown as ToolContext;
    const result = await askEditPermission(ctx, ["src/foo.ts"]);
    expect(result).toContain("OpenCode 1.15.5 or newer");
    expect(result).not.toContain("denied");
  });

  test("askEditPermission surfaces deny message when ask rejects", async () => {
    const ctx = makeMockContext(async () => {
      throw new Error("Permission denied for src/foo.ts");
    });
    const result = await askEditPermission(ctx, ["src/foo.ts"]);
    expect(result).toBe("Permission denied for src/foo.ts");
  });

  test("askEditPermission falls back to default message when ask rejects without a useful message", async () => {
    const ctx = makeMockContext(async () => {
      throw new Error("");
    });
    const result = await askEditPermission(ctx, ["src/foo.ts"]);
    expect(result).toBe("Permission denied.");
  });

  test("ask body actually executes — proves we did not regress to a no-op", async () => {
    // If runAsk ever became `async (_) => {}` (dropping the await), this fails
    // because the body of the ask Promise never runs to set the flag.
    let askWasInvoked = false;
    const ctx = makeMockContext(async () => {
      askWasInvoked = true;
    });
    await askEditPermission(ctx, ["src/foo.ts"]);
    expect(askWasInvoked).toBe(true);
  });
});

describe("askGrepPermission / askGlobPermission (Promise contract)", () => {
  test("askGrepPermission returns undefined on allow", async () => {
    const ctx = makeMockContext(async () => {});
    const result = await askGrepPermission(ctx, "TODO");
    expect(result).toBeUndefined();
  });

  test("askGrepPermission surfaces deny message", async () => {
    const ctx = makeMockContext(async () => {
      throw new Error("Grep denied by policy");
    });
    const result = await askGrepPermission(ctx, "TODO");
    expect(result).toBe("Grep denied by policy");
  });

  test("askGrepPermission falls back to default message when ask rejects without one", async () => {
    const ctx = makeMockContext(async () => {
      throw new Error("");
    });
    const result = await askGrepPermission(ctx, "TODO");
    expect(result).toBe("Permission denied (grep).");
  });

  test("askGrepPermission forwards pattern + path + include in the ask payload", async () => {
    let observed: { permission?: string; patterns?: string[]; metadata?: Record<string, unknown> } =
      {};
    const ctx = makeMockContext(async (args) => {
      observed = args as typeof observed;
    });
    await askGrepPermission(ctx, "TODO\\b", { path: "src", include: "*.ts" });
    expect(observed.permission).toBe("grep");
    expect(observed.patterns).toEqual(["TODO\\b"]);
    expect(observed.metadata).toEqual({ pattern: "TODO\\b", path: "src", include: "*.ts" });
  });

  test("askGlobPermission returns undefined on allow", async () => {
    const ctx = makeMockContext(async () => {});
    const result = await askGlobPermission(ctx, "**/*.ts");
    expect(result).toBeUndefined();
  });

  test("askGlobPermission surfaces deny message", async () => {
    const ctx = makeMockContext(async () => {
      throw new Error("Glob denied by policy");
    });
    const result = await askGlobPermission(ctx, "**/*.ts");
    expect(result).toBe("Glob denied by policy");
  });

  test("askGlobPermission falls back to default message when ask rejects without one", async () => {
    const ctx = makeMockContext(async () => {
      throw new Error("");
    });
    const result = await askGlobPermission(ctx, "**/*.ts");
    expect(result).toBe("Permission denied (glob).");
  });

  test("askGlobPermission forwards pattern + path in the ask payload", async () => {
    let observed: { permission?: string; patterns?: string[]; metadata?: Record<string, unknown> } =
      {};
    const ctx = makeMockContext(async (args) => {
      observed = args as typeof observed;
    });
    await askGlobPermission(ctx, "**/*.test.ts", { path: "src" });
    expect(observed.permission).toBe("glob");
    expect(observed.patterns).toEqual(["**/*.test.ts"]);
    expect(observed.metadata).toEqual({ pattern: "**/*.test.ts", path: "src" });
  });
});

function makeMockContext(askFn: ToolContext["ask"]): ToolContext {
  return {
    sessionID: "test-session",
    messageID: "test-message",
    agent: "test-agent",
    directory: "/tmp/aft-permissions-promise-test",
    worktree: "/tmp/aft-permissions-promise-test",
    abort: new AbortController().signal,
    metadata: () => {},
    ask: askFn,
  };
}
