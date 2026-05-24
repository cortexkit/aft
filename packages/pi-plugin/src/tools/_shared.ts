/**
 * Shared helpers used by every Pi tool wrapper.
 */

import type { BinaryBridge, BridgeRequestOptions } from "@cortexkit/aft-bridge";
import type { AgentToolResult, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { ingestBgCompletions } from "../bg-notifications.js";
import type { PluginContext } from "../types.js";

/**
 * Per-command timeout overrides (milliseconds).
 *
 * Commands not listed fall back to the bridge-wide default (30s). Only
 * extend budgets for operations that legitimately walk the project
 * file tree or wait on external I/O (embedding API, index build). The
 * goal is to absorb slow first-call spikes without masking real hangs.
 */
export const LONG_RUNNING_COMMAND_TIMEOUT_MS: Record<string, number> = {
  callers: 60_000,
  trace_to: 60_000,
  trace_data: 60_000,
  impact: 60_000,
  grep: 60_000,
  glob: 60_000,
  semantic_search: 45_000,
};

/** Returns the per-command timeout override, or undefined to use the bridge default. */
export function timeoutForCommand(command: string): number | undefined {
  return LONG_RUNNING_COMMAND_TIMEOUT_MS[command];
}

/** Get the session bridge for the current working directory. */
export function bridgeFor(ctx: PluginContext, cwd: string): BinaryBridge {
  return ctx.pool.getBridge(cwd);
}

/**
 * Resolve Pi's native session ID from the tool execution context so that
 * `/new`, `/fork`, and `/resume` each scope their own undo/checkpoint
 * namespace in AFT instead of sharing one extension-wide UUID.
 *
 * `sessionManager` is on every `ExtensionContext`; we read it defensively
 * because Pi's public type surface is still evolving and we don't want a
 * missing field at runtime to wedge tool execution.
 */
export function resolveSessionId(extCtx: ExtensionContext): string | undefined {
  const manager = (extCtx as unknown as { sessionManager?: { getSessionId?: () => string } })
    .sessionManager;
  const id = manager?.getSessionId?.();
  return typeof id === "string" && id.length > 0 ? id : undefined;
}

/**
 * Call a bridge command and throw a plain Error on failure.
 * Every tool handler should guard with `if (response.success === false)`
 * before accessing success-only fields — this helper does it uniformly.
 *
 * `extCtx` is used to derive Pi's current session ID per call so Rust
 * scopes backups/undo per Pi session rather than per extension instance.
 */
export async function callBridge(
  bridge: BinaryBridge,
  command: string,
  params: Record<string, unknown> = {},
  extCtx?: ExtensionContext,
  options?: BridgeRequestOptions,
): Promise<Record<string, unknown>> {
  const timeoutMs = timeoutForCommand(command);
  const merged: Record<string, unknown> = { ...params };
  const sessionId = extCtx ? resolveSessionId(extCtx) : undefined;
  if (sessionId) {
    merged.session_id = sessionId;
  }
  const sendOptions = {
    ...(timeoutMs !== undefined ? { timeoutMs } : {}),
    configureWarningClient: extCtx,
    ...options,
  };
  const response = await bridge.send(
    command,
    merged,
    Object.keys(sendOptions).length > 0 ? sendOptions : undefined,
  );
  if (response.success === false) {
    const message =
      typeof response.message === "string" && response.message.length > 0
        ? response.message
        : `${command} failed`;
    throw new Error(message);
  }
  ingestBgCompletions(sessionId, response.bg_completions);
  return response;
}

/**
 * Build a text-only AgentToolResult.
 * This is the standard result shape for most AFT tools.
 */
export function textResult<TDetails = unknown>(
  text: string,
  details?: TDetails,
): AgentToolResult<TDetails> {
  return {
    content: [{ type: "text", text }],
    details: details as TDetails,
  };
}

/**
 * Convert a bridge response into a pretty JSON string for the model.
 * Strips undefined/null fields that just clutter the output.
 */
export function jsonTextResult<TDetails = unknown>(
  response: Record<string, unknown>,
  details?: TDetails,
): AgentToolResult<TDetails> {
  return textResult(JSON.stringify(response, null, 2), details);
}

/** Strip top-level success field before JSON stringifying. */
export function stripSuccess(response: Record<string, unknown>): Record<string, unknown> {
  const { success: _success, ...rest } = response;
  return rest;
}
