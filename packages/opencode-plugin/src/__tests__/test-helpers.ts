/// <reference path="../bun-test.d.ts" />
/**
 * Shared test helpers for OpenCode plugin tests.
 *
 * As of `@opencode-ai/plugin@1.15.5`, `ToolContext.ask()` returns
 * `Promise<void>` again. These helpers give tests a Promise-shaped ask mock
 * so `runAsk` can `await` the result naturally.
 *
 * (Background: the SDK briefly used `Effect.Effect<void>` in 1.14.x–1.15.4.
 * AFT now targets the post-flip Promise contract; if the SDK ever changes
 * shape again, this file is the single place to adjust mocks.)
 */
import { mock } from "bun:test";
import type { ToolContext, ToolResult } from "@opencode-ai/plugin";

/**
 * Normalize a `ToolResult` (SDK >=1.14 widened this from `string` to
 * `string | { output: string }`) down to the agent-visible string for
 * test assertions like `.toContain` / `.toBe`.
 */
export function toolResultText(result: ToolResult): string {
  return typeof result === "string" ? result : result.output;
}

/** No-op `ctx.ask` that resolves cleanly through `runAsk`. */
export const noopAsk: ToolContext["ask"] = async () => {};

/**
 * Like `mock(async () => {})` but typed as `ToolContext["ask"]` so it can
 * be passed straight into the SDK shape. Use when a test needs to inspect
 * call args.
 */
export function mockAsk(): ReturnType<typeof mock> & ToolContext["ask"] {
  return mock(async () => {}) as unknown as ReturnType<typeof mock> & ToolContext["ask"];
}

/**
 * Build a Promise-shaped ask mock that rejects (simulating a deny). The
 * error message is what `askEditPermission` surfaces to callers.
 */
export function mockAskDeny(
  message: string = "Permission denied.",
): ReturnType<typeof mock> & ToolContext["ask"] {
  return mock(async () => {
    throw new Error(message);
  }) as unknown as ReturnType<typeof mock> & ToolContext["ask"];
}
