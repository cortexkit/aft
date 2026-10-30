/**
 * Shared helpers used by every Pi tool wrapper.
 */

import { AsyncLocalStorage } from "node:async_hooks";
import { existsSync } from "node:fs";
import type {
  AftProjectTransport,
  AftTransportPool,
  BridgeRequestOptions,
  ToolCallOptions,
  ToolCallResult,
} from "@cortexkit/aft-bridge";
import {
  adaptToolError,
  BASH_HOST_FALLBACK_REFUSAL,
  formatBridgeErrorMessage,
  isBashTransportDeadError,
  prepareCanonicalEditArguments,
  prepareCanonicalPathArguments,
  refuseAftModuleOptionWithoutFallback,
  timeoutForCommand,
  WORKER_SESSION_FIELD,
} from "@cortexkit/aft-bridge";
import type {
  AgentToolResult,
  ExtensionContext,
  ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { type Static, type TSchema, Type } from "typebox";

import { forwardPendingBashWaitDetach } from "../bash-wait-detach.js";
import { ingestBgCompletions } from "../bg-notifications.js";
import type { PiToolPresentation } from "../config.js";
import type { PiHarness } from "../harness.js";
import { detectPiHarness } from "../harness.js";
import { type OmpToolTier, ompApprovalFor } from "../omp-approval.js";
import { isPiWorkerSession } from "../session-kind.js";
import type { PluginContext } from "../types.js";

type TextContent = { type: "text"; text: string; textSignature?: string };
type ImageContent = { type: "image"; data: string; mimeType: string };
type ContentBlock = TextContent | ImageContent;

/**
 * Optional integer field schema for Pi tool parameters.
 *
 * Pi validates tool arguments against this schema BEFORE our handler runs, and
 * some models send stringified integers like "42". A strict `Type.Integer()`
 * would reject those calls before `coerceOptionalInt()` can normalize them, so
 * keep the schema permissive at the field level while still documenting the
 * real integer contract for models and host UIs.
 */
export const optionalInt = (min: number, max: number, description = "(integer)") =>
  Type.Optional(
    Type.Union([Type.Integer({ minimum: min, maximum: max }), Type.String()], {
      description,
    }),
  );

// Re-exported from @cortexkit/aft-bridge — shared runtime coercion,
// formatting, and timeout tables live in the host-neutral bridge package.
export {
  coerceOptionalInt,
  formatBridgeErrorMessage,
  isEmptyParam,
  LONG_RUNNING_COMMAND_TIMEOUT_MS,
  prepareCanonicalPathArguments,
  timeoutForCommand,
} from "@cortexkit/aft-bridge";

/** Attach Pi's raw-argument preparation hook to a path-bearing tool. */
export function withPathAliasPreparation<
  TParams extends TSchema,
  TDetails = unknown,
  TState = unknown,
>(tool: ToolDefinition<TParams, TDetails, TState>): ToolDefinition<TParams, TDetails, TState> {
  const existing = tool.prepareArguments;
  const prepare = (args: unknown): Static<TParams> => {
    const isEditTool = tool.name === "edit" || tool.name === "aft_edit";
    // Apply the same canonical edit handling to `aft_edit` and bare `edit` so
    // both registrations accept the same request shapes.
    const prepared = isEditTool
      ? prepareCanonicalEditArguments("edit", args)
      : prepareCanonicalPathArguments(tool.name, args);
    return (existing ? existing(prepared) : prepared) as Static<TParams>;
  };
  return {
    ...tool,
    prepareArguments: prepare,
    execute(toolCallId, params, signal, onUpdate, context) {
      return tool.execute(toolCallId, prepare(params), signal, onUpdate, context);
    },
  };
}

/**
 * Fold `promptSnippet` and `promptGuidelines` into `description` with a fixed layout:
 * description, blank line, snippet, guidelines as `- ` lines.
 */
export function foldToolGuidanceIntoDescription(
  description: string | undefined,
  snippet: string | undefined,
  guidelines: readonly string[] | undefined,
): string | undefined {
  const trimmedSnippet = snippet?.trim();
  const trimmedGuidelines = (guidelines ?? []).map((g) => g.trim()).filter((g) => g.length > 0);

  if (!trimmedSnippet && trimmedGuidelines.length === 0) {
    return description;
  }

  const sections: string[] = [];
  const base = description?.trim() ?? "";
  if (base.length > 0) {
    sections.push(base);
  }

  const guidanceLines: string[] = [];
  if (trimmedSnippet) {
    guidanceLines.push(trimmedSnippet);
  }
  for (const g of trimmedGuidelines) {
    guidanceLines.push(`- ${g}`);
  }

  if (guidanceLines.length > 0) {
    sections.push(guidanceLines.join("\n"));
  }

  return sections.join("\n\n");
}

export type PreparedToolDefinition<
  TParams extends TSchema = TSchema,
  TDetails = unknown,
  TState = unknown,
> = ToolDefinition<TParams, TDetails, TState> & {
  loadMode?: "essential" | "discoverable";
  /** OMP's capability tier for one call; see `omp-approval.ts`. */
  approval?: (args: unknown) => OmpToolTier;
};

/** The working directory of the session that last ran an AFT tool or started. */
let lastSessionCwd: string | undefined;

/**
 * Remember the session's project directory. OMP evaluates a tool's approval
 * tier before the call runs and without its context, so the tier is judged
 * against the most recent session directory, or the process directory before
 * any session reported one.
 */
export function recordSessionCwd(cwd: unknown): void {
  if (typeof cwd === "string" && cwd.length > 0) lastSessionCwd = cwd;
}

function sessionProjectRoot(): string | undefined {
  return lastSessionCwd ?? process.cwd();
}

/**
 * Funnel helper that applies harness-specific adjustments to a ToolDefinition:
 * - On OMP: attaches `loadMode: "essential"` when presentation is "top_level",
 *   folds `promptSnippet` and `promptGuidelines` into `description`, and
 *   declares the tool's approval tier (`ompApprovalFor`).
 *   Upstream Pi has no approval gate, so it gets no `approval` field.
 * - On Pi (or unknown): leaves definition fields untouched (Pi renders snippet
 *   and guidelines itself; unknown behaves as pi).
 */
export function prepareToolDefinitionForRegistration<
  TParams extends TSchema,
  TDetails = unknown,
  TState = unknown,
>(
  tool: ToolDefinition<TParams, TDetails, TState>,
  harness: PiHarness = "pi",
  presentation: PiToolPresentation = "top_level",
): PreparedToolDefinition<TParams, TDetails, TState> {
  const effectiveHarness = harness === "omp" ? "omp" : "pi";
  if (effectiveHarness !== "omp") {
    return tool;
  }

  const foldedDescription = foldToolGuidanceIntoDescription(
    tool.description,
    tool.promptSnippet,
    tool.promptGuidelines,
  );

  const prepare = tool.prepareArguments as ((args: unknown) => unknown) | undefined;
  const approval = ompApprovalFor(tool.name, sessionProjectRoot, prepare);

  return {
    ...tool,
    ...(foldedDescription !== undefined ? { description: foldedDescription } : {}),
    ...(presentation === "top_level" ? { loadMode: "essential" } : {}),
    ...(approval ? { approval } : {}),
  };
}

const bridgePools = new WeakMap<AftProjectTransport, AftTransportPool>();

/** Get the session bridge for the current working directory. */
export function bridgeFor(ctx: PluginContext, cwd: string): AftProjectTransport {
  // A restored session can point at a reclaimed mason worktree (or any
  // deleted directory). Binding it would make the module configure, lease,
  // and warm indexes for a dead root — refuse before any transport work.
  if (!existsSync(cwd)) {
    throw new Error(`project directory no longer exists: ${cwd} (stale restored session?)`);
  }
  // A tool call is a session using AFT: make sure the deferred startup (which
  // hands cached LSP server paths to new bridges) has run before this spawn.
  ctx.startSessionWork?.();
  const bridge = ctx.pool.getBridge(cwd);
  bridgePools.set(bridge, ctx.pool);
  return bridge;
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
 * Error thrown by callBridge on a `success: false` response. Carries the Rust
 * error `code` so callers can distinguish soft negatives (e.g. symbol_not_found)
 * from genuine errors without re-parsing the message.
 */
export class BridgeError extends Error {
  readonly code: string;
  readonly response?: Record<string, unknown>;
  constructor(message: string, code: string, response?: Record<string, unknown>) {
    super(message);
    this.name = "BridgeError";
    this.code = code;
    this.response = response;
  }
}

const hostPermissionGrants = new WeakMap<object, readonly string[]>();

/**
 * How AFT answers the engine's outside-access asks for one call.
 * - `harness`: on OMP, OMP's own approval mode decides through the tools'
 *   declared tiers, so AFT never asks there.
 * - `askOnPi`: the user's `pi.external_access_ask` setting (default on).
 */
export interface OutsideAccessPolicy {
  harness: PiHarness;
  askOnPi: boolean;
}

/** State private to one host `execute` call: preview continuations, the
 * call's own cancellation signal, and how outside-access asks are answered. */
interface PermissionOperation {
  continuations: Map<string, string[]>;
  signal?: AbortSignal;
  policy?: OutsideAccessPolicy;
}
const permissionOperations = new AsyncLocalStorage<PermissionOperation>();

/** Run one host `execute` call. `signal` is the call's own cancellation
 * signal (the one Pi passes to `execute`); an outside-access prompt for the
 * call stops on it as well as on the extension context's signal. A nested
 * call without its own `policy` keeps the enclosing call's. */
export function withOutsidePermissionOperation<T>(
  run: () => T,
  signal?: AbortSignal,
  policy?: OutsideAccessPolicy,
): T {
  const inherited = policy ?? permissionOperations.getStore()?.policy;
  return permissionOperations.run(
    { continuations: new Map(), signal, ...(inherited ? { policy: inherited } : {}) },
    run,
  );
}

/** Every signal that cancels this call: the extension context's and the
 * signal passed to the running tool's `execute`. */
function cancellationSignals(extCtx: ExtensionContext | undefined): AbortSignal[] {
  const signals = [extCtx?.signal, permissionOperations.getStore()?.signal];
  return signals.filter(
    (signal, index): signal is AbortSignal =>
      signal !== undefined && signals.indexOf(signal) === index,
  );
}

function throwIfCancelled(signals: AbortSignal[]): void {
  for (const signal of signals) signal.throwIfAborted();
}

function permissionOperation(tool: string, args: Record<string, unknown>): string {
  if (["hashline_preflight", "hashline_edit", "edit", "edit_symbol"].includes(tool)) return "edit";
  if (["undo_preview", "undo"].includes(tool)) return "safety:undo";
  if (["checkpoint_paths", "restore_checkpoint"].includes(tool)) return "safety:restore";
  if (tool === "checkpoint") return "safety:checkpoint";
  if (tool === "safety") return `safety:${String(args.op ?? "")}`;
  return (
    ({ read_file: "read", write_file: "write", delete_file: "delete" } as Record<string, string>)[
      tool
    ] ?? tool
  );
}

export function withHostPermissionGrants(args: Record<string, unknown>, grants: readonly string[]) {
  const { permissions_granted: _untrusted, ...clean } = args;
  hostPermissionGrants.set(clean, [...grants]);
  return clean;
}

/** Hard ceiling on sends for one call. The engine reports one refused outside
 * path per attempt, so a batch needs one round per distinct outside path plus
 * the final run; this is far above any real batch. */
export const MAX_OUTSIDE_PERMISSION_ROUNDS = 256;

/** How many times one operation and path may be asked again because the
 * user's approval arrived after its token expired. */
export const MAX_EXPIRED_REASKS_PER_PATH = 2;

/**
 * Whether a person can answer an outside-access prompt in this session. Pi
 * reports dialog-capable UI in RPC mode too, but there the prompt goes to the
 * RPC client, which may never answer it, so only Pi's own terminal UI counts.
 * Pi releases without `mode` keep the `hasUI` answer.
 */
export function canPromptForOutsideAccess(
  extCtx: Pick<ExtensionContext, "hasUI" | "ui"> & { mode?: string },
): boolean {
  if (!extCtx.hasUI || typeof extCtx.ui?.confirm !== "function") return false;
  return extCtx.mode === undefined || extCtx.mode === "tui";
}

/**
 * Whether AFT asks the user before an outside access, or approves the
 * engine's ask itself. Approving itself is the behaviour from before AFT
 * asked at all: outside access is allowed, and the engine's floors
 * (credentials, AFT's data folder, `sandbox.read_deny`) and a configured
 * `restrict_to_project_root` still refuse, because those are denials, never
 * asks. AFT asks only on plain Pi, with `pi.external_access_ask` on, in Pi's
 * own terminal UI; print, JSON and RPC sessions are never asked, so nothing
 * waits on an answer.
 */
export function promptsForOutsideAccess(
  extCtx: ExtensionContext | undefined,
  policy: OutsideAccessPolicy,
): boolean {
  if (policy.harness === "omp" || !policy.askOnPi) return false;
  return extCtx !== undefined && canPromptForOutsideAccess(extCtx);
}

/** The policy for a call made outside a registered tool's `execute`. */
function defaultOutsideAccessPolicy(): OutsideAccessPolicy {
  return { harness: detectPiHarness(), askOnPi: true };
}

/** An engine ask for one outside path: kind `external_directory` with an `outside:` grant ID. */
export function isOutsideAccessAsk(ask: unknown): boolean {
  if (ask === null || typeof ask !== "object") return false;
  const { kind, grant_id: grantId } = ask as { kind?: unknown; grant_id?: unknown };
  return (
    kind === "external_directory" && typeof grantId === "string" && grantId.startsWith("outside:")
  );
}

/** The outside-access policy `ctx`'s current config selects on `harness`. */
export function outsideAccessPolicyFor(
  ctx: PluginContext,
  harness: PiHarness = detectPiHarness(),
): OutsideAccessPolicy {
  return { harness, askOnPi: ctx.config.pi?.external_access_ask ?? true };
}

/** The policy of the registered tool call running now, if any. */
export function currentOutsideAccessPolicy(): OutsideAccessPolicy | undefined {
  return permissionOperations.getStore()?.policy;
}

/**
 * The engine owns containment; Pi only answers its exact outside-path asks,
 * by asking the user or, where `promptsForOutsideAccess` says no one is to be
 * asked, by approving them itself.
 */
export async function withOutsidePermissions<T extends Record<string, unknown>>(
  args: Record<string, unknown>,
  extCtx: ExtensionContext | undefined,
  send: (args: Record<string, unknown>) => Promise<T>,
  preview = false,
  operation = "",
  sessionId = "",
): Promise<T> {
  const { permissions_granted: _untrusted, ...clean } = args;
  const inherited = hostPermissionGrants.get(args) ?? [];
  const operationState = permissionOperations.getStore();
  const continuations = operationState?.continuations;
  const prompt = promptsForOutsideAccess(
    extCtx,
    operationState?.policy ?? defaultOutsideAccessPolicy(),
  );
  const signals = cancellationSignals(extCtx);
  const key = JSON.stringify([sessionId, permissionOperation(operation, clean)]);
  let granted = [...(continuations?.get(key) ?? [])];
  continuations?.delete(key);
  // What this call has approved so far, by operation and path. A batch keeps
  // retrying while each round approves something new; the engine asking
  // again for something already approved means retrying cannot finish.
  const approvedThisCall = new Set<string>();
  // Re-asks the engine marked `approval_expired`, per operation and path:
  // the user said yes after the token expired, so asking again is progress.
  const expiredReasks = new Map<string, number>();
  // A reconnect can replace the route while its prompt is open. Offer one
  // fresh approval, but do not loop through repeated reconnects in one call.
  const replacedRouteReasks = new Set<string>();
  for (let round = 0; ; round++) {
    // Before every send, including an approved retry: a call cancelled
    // while it waited for an answer must not run.
    throwIfCancelled(signals);
    if (round >= MAX_OUTSIDE_PERMISSION_ROUNDS)
      throw new BridgeError(
        `outside-access permission retry stopped after ${MAX_OUTSIDE_PERMISSION_ROUNDS} rounds`,
        "outside_permission_stalled",
      );
    const approvals = [...inherited, ...granted];
    const response = await send(
      approvals.length ? { ...clean, permissions_granted: approvals } : clean,
    );
    granted = Array.isArray(response.permissions_continuation)
      ? response.permissions_continuation.filter(
          (id): id is string => typeof id === "string" && id.startsWith("outside:"),
        )
      : [];
    delete response.permissions_continuation;
    if (response.success !== false || response.code !== "permission_required") {
      if (preview && response.success !== false && granted.length) continuations?.set(key, granted);
      return response;
    }
    const asks = response.asks as
      | Array<{
          kind: string;
          grant_id?: string;
          path?: string;
          operation?: string;
          reason?: string;
        }>
      | undefined;
    // Only outside-path asks are answered here, and only they are ever
    // approved without a prompt. Any other ask (a bash rule, an escalation, a
    // kind a later engine adds) goes back to the caller untouched, so it can
    // never be approved silently.
    if (!asks?.length || !asks.every(isOutsideAccessAsk)) {
      if (granted.length) continuations?.set(key, granted);
      return response;
    }
    // The same operation on the same path is the same request. An ask that
    // names no path can only be told apart by its own grant ID.
    const askKey = (ask: (typeof asks)[number]) =>
      ask.path === undefined
        ? JSON.stringify(["grant", ask.grant_id])
        : JSON.stringify([ask.operation ?? "", ask.path]);
    for (const ask of asks) {
      const key = askKey(ask);
      if (!approvedThisCall.has(key)) continue;
      const where = ask.path ?? "an outside path";
      const replacedRoute = ask.reason === "approval_route_replaced";
      // A fresh route needs fresh approval; no token crosses the route boundary.
      // Any other refusal, or a second replacement, cannot finish this call.
      if (
        (ask.reason !== "approval_expired" && !replacedRoute) ||
        (replacedRoute && replacedRouteReasks.has(key))
      )
        throw new BridgeError(
          `outside access to ${where} was refused again after it was approved`,
          "outside_permission_stalled",
        );
      if (replacedRoute) {
        replacedRouteReasks.add(key);
        continue;
      }
      const reasked = expiredReasks.get(key) ?? 0;
      if (reasked >= MAX_EXPIRED_REASKS_PER_PATH)
        throw new BridgeError(
          `outside access to ${where} kept expiring before it was approved`,
          "outside_permission_stalled",
        );
      expiredReasks.set(key, reasked + 1);
    }
    for (const ask of asks) {
      throwIfCancelled(signals);
      if (prompt && extCtx) {
        const approved = await extCtx.ui.confirm(
          "Allow outside access?",
          `${ask.operation ?? "access"}: ${ask.path ?? ""}`,
          { signal: signals.length > 1 ? AbortSignal.any(signals) : signals[0] },
        );
        // An answer that arrives after cancellation is ignored.
        throwIfCancelled(signals);
        if (!approved)
          throw new BridgeError(
            "Permission denied: outside access was denied.",
            "permission_denied",
          );
      }
      approvedThisCall.add(askKey(ask));
      const id = ask.grant_id;
      if (id && !granted.includes(id)) granted.push(id);
    }
  }
}

/**
 * Call a bridge command and throw a BridgeError on failure.
 * Every tool handler should guard with `if (response.success === false)`
 * before accessing success-only fields — this helper does it uniformly.
 *
 * `extCtx` is used to derive Pi's current session ID per call so Rust
 * scopes backups/undo per Pi session rather than per extension instance.
 */
export async function callBridge(
  bridge: AftProjectTransport,
  command: string,
  params: Record<string, unknown> = {},
  extCtx?: ExtensionContext,
  options?: BridgeRequestOptions,
): Promise<Record<string, unknown>> {
  const timeoutMs = timeoutForCommand(command);
  const merged: Record<string, unknown> = { ...params };
  const trustedGrants = hostPermissionGrants.get(params);
  if (trustedGrants) hostPermissionGrants.set(merged, trustedGrants);
  const sessionId = extCtx ? resolveSessionId(extCtx) : undefined;
  if (sessionId) {
    merged.session_id = sessionId;
  }
  // The caller's role travels with every request, beside the session; AFT
  // words its replies by it (see WORKER_SESSION_FIELD).
  if (isPiWorkerSession(extCtx)) {
    merged[WORKER_SESSION_FIELD] = true;
  }
  const sendOptions = {
    ...(timeoutMs !== undefined ? { timeoutMs } : {}),
    configureWarningClient: extCtx,
    ...options,
  };
  let response: Record<string, unknown>;
  try {
    const pool = bridgePools.get(bridge);
    if (pool) await forwardPendingBashWaitDetach(pool, bridge, sessionId, command === "bash");
    response = await withOutsidePermissions(
      merged,
      extCtx,
      (approved) =>
        bridge.send(
          command,
          approved,
          Object.keys(sendOptions).length > 0 ? sendOptions : undefined,
        ),
      ["undo_preview", "checkpoint_paths", "hashline_preflight"].includes(command),
      command,
      sessionId,
    );
  } catch (error) {
    // Host fallback and gate-off propagation both require the untouched bridge
    // transport error; logical AFT responses are handled below as BridgeError.
    if (command === "bash" && isBashTransportDeadError(error)) throw error;
    throw adaptToolError(command, error);
  }
  if (response.success === false) {
    const pathRefusal =
      command === "bash" &&
      response.code === "runon_cd_outside_checkout" &&
      typeof response.message === "string" &&
      response.message.length > 0;
    throw new BridgeError(
      pathRefusal
        ? (response.message as string)
        : formatBridgeErrorMessage(command, response, merged),
      typeof response.code === "string" ? response.code : "",
      response,
    );
  }
  ingestBgCompletions(sessionId, response.bg_completions);
  return response;
}

/**
 * Wrapper that calls a tool on the Pi agent. It supplies the session ID and
 * timeout, forwards warnings, gathers any follow-up data, and returns the raw
 * response plus the text summary the model will receive.
 */
export async function callToolCall(
  bridge: AftProjectTransport,
  name: string,
  rawArgs: Record<string, unknown> = {},
  extCtx?: ExtensionContext,
  options?: ToolCallOptions,
): Promise<ToolCallResult> {
  return callToolCallForSession(
    bridge,
    name,
    rawArgs,
    extCtx ? resolveSessionId(extCtx) : undefined,
    extCtx,
    options,
  );
}

/**
 * Dispatch one bridge call with a session identity captured by the surrounding
 * logical tool operation. Multi-stage operations must not resolve Pi's mutable
 * session manager again after an await, because another active session can
 * become current between preflight, preview, and apply.
 */
export async function callToolCallForSession(
  bridge: AftProjectTransport,
  name: string,
  rawArgs: Record<string, unknown>,
  sessionId: string | undefined,
  extCtx?: ExtensionContext,
  options?: ToolCallOptions,
): Promise<ToolCallResult> {
  const timeoutMs = timeoutForCommand(name);
  const sendOptions = {
    ...(timeoutMs !== undefined ? { timeoutMs } : {}),
    configureWarningClient: extCtx,
    // The caller's role travels in the tool_call envelope, beside the session.
    ...(isPiWorkerSession(extCtx) ? { workerSession: true } : {}),
    ...options,
  };
  let response: ToolCallResult;
  try {
    const pool = bridgePools.get(bridge);
    if (pool) await forwardPendingBashWaitDetach(pool, bridge, sessionId, name === "bash");
    response = await withOutsidePermissions(
      rawArgs,
      extCtx,
      (approved) =>
        bridge.toolCall(
          sessionId,
          name,
          approved,
          Object.keys(sendOptions).length > 0 ? sendOptions : undefined,
        ),
      options?.preview === true ||
        ["undo_preview", "checkpoint_paths", "hashline_preflight"].includes(name),
      name,
      sessionId,
    );
  } catch (error) {
    const noFallbackOption =
      name === "bash"
        ? rawArgs.runon !== undefined
          ? "runon"
          : rawArgs.pty === true
            ? "pty:true"
            : rawArgs.background === true
              ? "background:true"
              : undefined
        : undefined;
    if (noFallbackOption && isBashTransportDeadError(error)) {
      throw refuseAftModuleOptionWithoutFallback(
        error,
        noFallbackOption,
        BASH_HOST_FALLBACK_REFUSAL,
      );
    }
    throw adaptToolError(name, error);
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
  return contentResult([{ type: "text", text }], details);
}

/** Build an AgentToolResult that can include image content blocks. */
export function contentResult<TDetails = unknown>(
  content: ContentBlock[],
  details?: TDetails,
): AgentToolResult<TDetails> {
  return {
    content,
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
