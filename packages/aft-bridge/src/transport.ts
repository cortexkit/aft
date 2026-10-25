import type { BridgeRequestOptions, StatusSnapshot } from "./bridge.js";
import type { CanonicalRootPath } from "./lifecycle-registry.js";
import type { BridgeToolCallRuntime } from "./pool.js";

export type ToolCallArguments = Record<string, unknown>;

export interface ToolCallResult extends Record<string, unknown> {
  /** Server-rendered agent-facing output added by the `tool_call` command. */
  text: string;
  /** Direct bridge response success flag, carried through unchanged. */
  success: boolean;
  code?: string;
  message?: string;
  bg_completions?: unknown;
}

export interface AftTransportOptions extends BridgeRequestOptions {
  /** Per-call command timeout passed through to BinaryBridge.send. */
  timeoutMs?: number;
  /** Host client used for asynchronous configure-warning delivery. */
  configureWarningClient?: unknown;
  /** Configure command lifecycle override used by BinaryBridge.send. */
  markConfiguredOnSuccess?: boolean;
}

export interface ToolCallOptions extends AftTransportOptions {
  /** Host context stays outside agent arguments and is sent only for search. */
  searchContext?: string;
  /** Server-owned dry-run flag placed at the top level of the tool_call request. */
  preview?: boolean;
  /**
   * The caller is a delegated worker (subagent) session. Placed beside the
   * session, outside the agent's arguments, as `worker_session: true`; see
   * WORKER_SESSION_FIELD.
   */
  workerSession?: boolean;
  /**
   * The names the caller's host registered AFT's command and patch tools
   * under, placed beside the session as HOST_TOOL_NAMES_FIELD; see there.
   */
  hostToolNames?: HostToolNames;
}

/**
 * What a host calls AFT's `bash` and `apply_patch` tools, keyed by AFT's own
 * name. OpenCode 2 registers them as `shell` and `patch`.
 */
export type HostToolNames = Readonly<Partial<Record<"bash" | "apply_patch", string>>>;

/**
 * Request field that tells AFT the caller is a delegated worker (subagent)
 * session rather than a primary one. A worker cannot be woken once its turn
 * ends, so AFT words its replies accordingly: it never promises a completion
 * reminder or tells a worker to end its turn, and a worker's `wait: true`
 * bash call runs without the default hard kill. The plugins set it on every
 * request from a worker, next to `session_id`; absent means primary.
 */
export const WORKER_SESSION_FIELD = "worker_session";

/**
 * Request field that tells AFT what the caller's host calls AFT's command and
 * patch tools, so text AFT writes for the agent ("Prefer `read` tool over
 * bash.") names the tool the agent can call. It belongs to the caller, not the
 * project: one AFT process serves every host and session bound to a root, so
 * the plugin sets it on each request, next to `session_id`, like
 * WORKER_SESSION_FIELD. Absent means AFT's own names, `bash` and `apply_patch`.
 */
export const HOST_TOOL_NAMES_FIELD = "host_tool_names";

/**
 * Tool-call body field naming the catalog preset the caller runs under
 * (subc-protocol 0.29 `ToolCallRequest.preset`). The module refuses a tool
 * call that names no preset on a route the daemon stamped with a scope, so
 * the subc transport names one on every call: `worker` for a worker session,
 * `head` otherwise. It travels beside the call, never in the arguments.
 */
export const PRESET_FIELD = "preset";

/** The catalog preset for a call from a worker or a primary session. */
export function callPresetFor(workerSession: boolean): "worker" | "head" {
  return workerSession ? "worker" : "head";
}

// A single project's transport (today: one BinaryBridge per project root).
export interface AftProjectTransport {
  send(
    command: string,
    params?: Record<string, unknown>,
    options?: AftTransportOptions,
  ): Promise<Record<string, unknown>>;
  toolCall(
    sessionId: string | undefined,
    name: string,
    rawArgs?: ToolCallArguments,
    options?: ToolCallOptions,
  ): Promise<ToolCallResult>;
  getCwd(): string;
  getCachedStatus(): StatusSnapshot | null;
  cacheStatusSnapshot(snapshot: StatusSnapshot): void;
}

// The pool of project transports (today: BridgePool).
export interface AftTransportPool {
  /** Host-observed creation, never resume/reload. Optional for older pool implementations. */
  observeSessionStart?(projectRoot: string, session: string): void;
  getBridge(projectRoot: string): AftProjectTransport;
  /** Reuse a root already canonicalized for this request; raw paths need getBridge's symlink resolution. */
  getBridgeForCanonicalRoot?(projectRoot: CanonicalRootPath): AftProjectTransport;
  /** Dispatch with a root resolved by the host for this request, not a cached raw-path identity. */
  sendForCanonicalRoot?(
    projectRoot: CanonicalRootPath,
    command: string,
    params?: Record<string, unknown>,
    options?: AftTransportOptions,
  ): Promise<Record<string, unknown>>;
  toolCallForCanonicalRoot?(
    projectRoot: CanonicalRootPath,
    sessionId: string | undefined,
    name: string,
    rawArgs?: ToolCallArguments,
    options?: ToolCallOptions,
  ): Promise<ToolCallResult>;
  getActiveBridgeForRoot(projectRoot: string): AftProjectTransport | null;
  /** Observe concrete eviction without creating a replacement bridge. */
  subscribeBridgeEvictions?(listener: (canonicalRoot: string) => void): () => void;
  /**
   * All currently-live project transports, for session-scoped signals (e.g.
   * bash wait-detach) that must reach their target even when the caller's
   * root-key resolution disagrees with the key the tool call used. Callers
   * must only send session-scoped, read-only commands through these.
   */
  activeBridges(): AftProjectTransport[];
  toolCall(
    projectRoot: string,
    runtime: BridgeToolCallRuntime,
    name: string,
    rawArgs?: ToolCallArguments,
    options?: ToolCallOptions,
  ): Promise<ToolCallResult>;
  setConfigureOverride(key: string, value: unknown): void;
  /** Reapply configure-time overrides to an already-live project bridge. */
  reconfigure(projectRoot: string, overrides: Record<string, unknown>): Promise<void>;
  replaceBinary(path: string): Promise<string>;
  /** True when this pool instance has reached its terminal shutdown state. */
  isShutdown(): boolean;
  /** Shut down this pool, retaining the reason for any later revival diagnostic. */
  shutdown(reason?: string): Promise<void>;
  /**
   * Release any per-session transport state for `(projectRoot, session)` on
   * session end. Standalone (BridgePool) is a no-op — its bridges are per-project
   * and session state lives Rust-side. Subc tears down the session's tool + bg
   * routes. Idempotent.
   */
  closeSession(projectRoot: string, session: string): Promise<void>;
}

/** A host startup/reload/resume does not prove a session has no pre-existing work. */
export function observeFreshSessionStart(
  pool: AftTransportPool,
  root: string,
  session: string | undefined,
  reason: unknown,
): void {
  if (session && (reason === "new" || reason === "fork")) pool.observeSessionStart?.(root, session);
}

export interface AftTransport<ToolCallContext = string | undefined> {
  /** Lifecycle and raw-command path; tool dispatch uses toolCall instead. */
  send(
    command: string,
    params?: Record<string, unknown>,
    opts?: AftTransportOptions,
  ): Promise<Record<string, unknown>>;

  /**
   * Dispatch a hoisted agent tool through the shared server-side `tool_call`
   * command and return the full raw response, including sidecars.
   */
  toolCall(
    context: ToolCallContext,
    name: string,
    rawArgs: ToolCallArguments,
    opts?: ToolCallOptions,
  ): Promise<ToolCallResult>;
}
