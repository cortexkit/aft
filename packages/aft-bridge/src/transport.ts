import type { BridgeRequestOptions, StatusSnapshot } from "./bridge.js";
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
  /** Server-owned dry-run flag placed at the top level of the tool_call request. */
  preview?: boolean;
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
  getBridge(projectRoot: string): AftProjectTransport;
  getActiveBridgeForRoot(projectRoot: string): AftProjectTransport | null;
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
  shutdown(): Promise<void>;
  /**
   * Release any per-session transport state for `(projectRoot, session)` on
   * session end. Standalone (BridgePool) is a no-op — its bridges are per-project
   * and session state lives Rust-side. Subc tears down the session's tool + bg
   * routes. Idempotent.
   */
  closeSession(projectRoot: string, session: string): Promise<void>;
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
