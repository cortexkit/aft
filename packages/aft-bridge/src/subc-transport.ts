/**
 * Subconscious (subc) transport — the daemon-backed alternative to the standalone
 * NDJSON {@link BinaryBridge}. Implements the SAME {@link AftProjectTransport} /
 * {@link AftTransportPool} interfaces the plugins consume, so the entire tool /
 * hoisting / permission / UI surface stays transport-agnostic: only the ONE
 * construction site (BridgePool vs SubcTransportPool) differs.
 *
 * Standalone model: one `aft` child process per project root, session passed
 * per call. Subc model: ONE {@link SubcClient} per process (one authenticated
 * daemon connection), and a route opened+cached per `(project_root, harness,
 * session)` triple — exactly subc's {@link BindIdentity}. So the "pool" here is a
 * route cache over a single client, not N child processes.
 *
 * This module is S2 of B-FINAL: the tool-call route only. The bg_events idle-wake
 * subscription (S3) and the config gate that selects this transport (S4) build on
 * top of it. subc-client is a build-time path dependency bundled into the
 * published plugin dist; it is never a published runtime dependency.
 */

import { statSync } from "node:fs";

import type { RouteHandle } from "@cortexkit/subc-client";
import {
  AuthError,
  type BindIdentity,
  type ConsumerIdentity,
  type ControlPush,
  connectionFileExists,
  isConsumerReconnectTransient,
  type RequestOptions,
  type RouteOpenOptions,
  type RouteScope,
  type RouteTarget,
  routeCloseReason,
  SocketClosedError,
  SocketTimeoutError,
  StaleRouteHandleError,
  SubcCallError,
  SubcClient,
  SubcError,
} from "@cortexkit/subc-client";

import { log, warn } from "./active-logger.js";
import { isTerminalStatus } from "./bash-format.js";
import type { StatusSnapshot } from "./bridge.js";
import { isRouteOpenReloadWindowError, isRouteRequestReloadRefusal } from "./error-contract.js";
import {
  asCanonicalRootPath,
  asRootGeneration,
  type CanonicalRootPath,
  type ConcretePoolId,
  type LifecycleEvent,
  type LifecyclePoolRegistration,
  type LifecyclePoolRegistrationOptions,
  LifecycleRegistry,
  type RootGeneration,
} from "./lifecycle-registry.js";
import { canonicalizeProjectRoot } from "./project-identity.js";
import type {
  AftProjectTransport,
  AftTransportOptions,
  AftTransportPool,
  ToolCallArguments,
  ToolCallOptions,
  ToolCallResult,
} from "./transport.js";
import {
  callPresetFor,
  HOST_TOOL_NAMES_FIELD,
  PRESET_FIELD,
  WORKER_SESSION_FIELD,
} from "./transport.js";

/** The subc pool is closing and cannot carry another request. */
export class SubcTransportShuttingDownError extends SubcCallError {
  constructor() {
    super("terminal", "subc transport is shutting down", "transport_shutting_down");
    this.name = "SubcTransportShuttingDownError";
  }
}

/**
 * Why the plugin itself closed a route that still carried a request. Each value
 * is shown to the agent, so it names the plugin-side event in plain words.
 */
export type SubcLocalRouteCloseReason =
  | "transport shutting down"
  | "session closed"
  | "project root closed"
  | "route replaced"
  | "route closed by the plugin";

/**
 * The plugin closed one of its own routes (pool shutdown, session close,
 * project-root close, or a sibling call on the same session discarding the
 * shared route) while this call's request was already in flight on it.
 *
 * subc-client records a request as pending and starts writing its frame in the
 * same synchronous step, and its `closeRoute` rejects exactly those pending
 * requests. So a request rejected that way was written, or queued to be
 * written, and the daemon may have run it: the outcome is unknown. The call
 * must not be blind-retried, and bash must not re-run it on the host.
 */
export class SubcRouteClosedMidCallError extends SubcCallError {
  readonly reason: SubcLocalRouteCloseReason;

  constructor(reason: SubcLocalRouteCloseReason, cause: unknown) {
    super(
      "outcome_unknown",
      `the command may have run; the route closed mid-call: ${reason}`,
      "route_closed",
      cause,
    );
    this.name = "SubcRouteClosedMidCallError";
    this.reason = reason;
  }
}

/**
 * A data-plane request was queued and sent, but reading the daemon's response
 * frame timed out waiting for body bytes after the header was received.
 *
 * subc-client raises SocketTimeoutError from readExact when the body does not
 * arrive within BODY_READ_TIMEOUT_MS after the header. Because the request was
 * already sent to the daemon, it may have executed: the outcome is unknown.
 * The call must not be blind-retried, and bash must not re-run it on the host.
 */
export class SubcFrameReadTimeoutError extends SubcCallError {
  constructor(cause: unknown) {
    super(
      "outcome_unknown",
      "the command may have run; timed out waiting for frame body bytes from the AFT daemon",
      "frame_read_timeout",
      cause,
    );
    this.name = "SubcFrameReadTimeoutError";
  }
}

/**
 * Matches the frame-body read timeout raised by subc-client (socket.ts:185/190):
 * "timed out waiting for <N> bytes".
 *
 * Excludes connect timeouts ("timed out connecting to ...") and write timeouts
 * ("timed out writing to subc"): connect timeouts are provably pre-send, while
 * write timeouts keep their existing transport classification.
 */
function isFrameBodyReadTimeoutMessage(message: unknown): boolean {
  return (
    typeof message === "string" &&
    (/timed out waiting for \d+ bytes/i.test(message) ||
      message.includes("timed out waiting for frame body bytes"))
  );
}

export function isSubcFrameReadTimeoutError(error: unknown): boolean {
  if (error instanceof SubcFrameReadTimeoutError) return true;
  if (error instanceof SocketTimeoutError && isFrameBodyReadTimeoutMessage(error.message)) {
    return true;
  }
  if (error instanceof SubcError && isFrameBodyReadTimeoutMessage(error.message)) {
    return true;
  }
  if (error instanceof SubcCallError) {
    if (error.code === "frame_read_timeout") return true;
    if (
      error.cause !== undefined &&
      error.cause !== error &&
      isSubcFrameReadTimeoutError(error.cause)
    ) {
      return true;
    }
    if (isFrameBodyReadTimeoutMessage(error.message)) return true;
  }
  return false;
}

export function frameReadTimeoutError(error: unknown): unknown {
  if (!isSubcFrameReadTimeoutError(error)) return error;
  return error instanceof SubcFrameReadTimeoutError ? error : new SubcFrameReadTimeoutError(error);
}

/**
 * subc-client's rejection of a request that was pending when the client itself
 * closed the route: `closeRoute` tearing that route down, or the client closing
 * its connection. The client types both with `closeReason: "closed_by_caller"`;
 * a daemon GOODBYE carries the daemon's reason and a lost connection carries
 * `"connection_lost"`. A route closed with no request pending produces no error
 * at all, and a request started on an already-closed route fails with
 * `StaleRouteHandleError`. A route closed while still opening is reported as a
 * not-sent `SubcCallError`, which is not a `SubcError` and so never matches.
 */
export function isLocalRouteCloseRejection(error: unknown): error is SubcError {
  return error instanceof SubcError && error.closeReason === "closed_by_caller";
}

/** The route-close reason that matches a session's teardown, if it was torn down. */
function teardownCloseReason(
  teardown: SessionRecord["teardownReason"],
): SubcLocalRouteCloseReason | null {
  switch (teardown) {
    case "shutdown":
      return "transport shutting down";
    case "session_closed":
      return "session closed";
    case "root_reaped":
      return "project root closed";
    default:
      return null;
  }
}

class ReconnectKeyChangedError extends Error {
  constructor() {
    super(
      "the subc daemon restarted and its connection key changed; the reconnect did not complete within the deadline, retry",
    );
    this.name = "ReconnectKeyChangedError";
  }
}

/** True when subc-client rejected locally because its connection was already closed. */
export function isSubcClientClosedError(error: unknown): error is SubcError {
  return error instanceof SubcError && error.message === "client closed";
}

/**
 * True only when an error is evidence that the shared socket itself is dead, so
 * every route on it is gone and the client must be replaced.
 *
 * subc-client's `isConsumerReconnectTransient` answers a different question
 * ("may a managed call reconnect and retry?") and also accepts a managed
 * `SubcCallError` of kind `not_sent` / `outcome_unknown`. Those kinds are the
 * verdict on ONE call: a call whose deadline passed while the socket stayed up
 * is `outcome_unknown` too. So a `SubcCallError` counts as socket death only when
 * the error it wraps does. Everything else defers to the client's classifier,
 * whose remaining members are all socket-level: the socket closed or reset, a
 * write that could not complete, a handshake/auth failure while reconnecting,
 * ECONN and EPIPE errno codes. The client's own "client closed" rejection means
 * the connection has already been torn down locally.
 */
export function isSocketDeathError(error: unknown, depth = 0): boolean {
  if (isSubcFrameReadTimeoutError(error)) return true;
  // Raw data-plane requests wrap socket death in a SubcError; its typed
  // connection-loss reason survives even when the underlying errno does not.
  if (error instanceof SubcError && error.closeReason === "connection_lost") return true;
  if (error instanceof SubcCallError) {
    const cause = error.cause;
    return depth < 4 && cause !== undefined && cause !== error
      ? isSocketDeathError(cause, depth + 1)
      : false;
  }
  return isConsumerReconnectTransient(error) || isSubcClientClosedError(error);
}

/** Typed system codes for a reset or broken connected socket. */
const REQUEST_CONNECTION_LOSS_CODES = new Set(["ECONNABORTED", "ECONNRESET", "EPIPE"]);

function hasRequestConnectionLossCode(error: unknown): boolean {
  if (typeof error !== "object" || error === null || !("code" in error)) return false;
  return typeof error.code === "string" && REQUEST_CONNECTION_LOSS_CODES.has(error.code);
}

function hasRequestConnectionLossEvidence(error: unknown, depth = 0): boolean {
  if (error instanceof SocketClosedError) return true;
  if (error instanceof SubcError && error.closeReason === "connection_lost") return true;
  if (hasRequestConnectionLossCode(error)) return true;
  if (
    !(error instanceof SubcCallError) ||
    error.kind !== "outcome_unknown" ||
    depth >= 4 ||
    error.cause === undefined ||
    error.cause === error
  ) {
    return false;
  }
  return hasRequestConnectionLossEvidence(error.cause, depth + 1);
}

/**
 * A raw request first checks `closedErr`, then records itself pending and starts
 * `writeBorrowed` in the same synchronous send(). A later read-loop failure goes
 * through fail(), which rejects route-pending requests as SubcError with
 * closeReason="connection_lost". That proves the request write was attempted,
 * but the raw request API does not expose whether the socket queued those bytes;
 * the outcome must therefore stay unknown, not be retried.
 *
 * The timer path is different: it rejects with code="request_deadline" while
 * leaving the shared socket potentially live. Read/body timeouts reach fail() and
 * get the connection-lost close reason; a direct SocketTimeoutError from writing
 * can be before or after queueing. The socket's queued/not-queued write wrappers
 * are stripped by the raw write helper, so neither a bare timeout nor a bare
 * SocketClosedError alone identifies the write state. The latter remains
 * supported for older clients that rejected pending calls with that class.
 * Explicit kind="not_sent" is authoritative and always wins over these shapes.
 */
function connectionClosedAfterDispatch(error: unknown): unknown {
  if (error instanceof SubcCallError && error.kind === "not_sent") return error;
  if (typeof error === "object" && error !== null && "kind" in error && error.kind === "not_sent") {
    return error;
  }
  if (!hasRequestConnectionLossEvidence(error)) return error;
  if (error instanceof SubcCallError) return error;
  if (!(error instanceof Error)) return error;
  return Object.assign(error, { kind: "outcome_unknown" as const });
}

/**
 * Requests that subc-client's own deadline rejected after the request had been
 * handed to `client.request`. Unlike the same `request_deadline` code on a
 * route.open (which proves nothing was sent), the request may have reached AFT
 * and started running, so its outcome is unknown. Kept outside the error
 * object so the error's class, code, message and retry handling stay as
 * subc-client made them.
 */
const dispatchedRequestDeadlines = new WeakSet<object>();

function noteDispatchedRequestDeadline(error: unknown): void {
  if (error instanceof SubcError && error.code === REQUEST_DEADLINE_CODE) {
    dispatchedRequestDeadlines.add(error);
  }
}

/**
 * True for a request that ran out of subc-client's deadline after it was sent,
 * so AFT may have started it (see `dispatchedRequestDeadlines`).
 */
export function isDispatchedRequestDeadlineError(error: unknown): boolean {
  return typeof error === "object" && error !== null && dispatchedRequestDeadlines.has(error);
}

/**
 * A call that never got an answer on a socket that may still be alive: the
 * client's own request deadline (raw `request`/`routeOpen` reject with code
 * `request_deadline`) or a managed verdict of `not_sent` / `outcome_unknown`
 * that is not backed by socket death. Several of these in a row are grounds to
 * check the connection, never to close it on their own.
 */
function isUnansweredCallError(error: unknown): boolean {
  if (isSocketDeathError(error)) return false;
  if (error instanceof SubcCallError) return error.kind !== "terminal";
  return error instanceof SubcError && error.code === REQUEST_DEADLINE_CODE;
}

/**
 * A rejection that the daemon itself sent back (a coded wire Error frame, such as
 * a refused route.open), which proves the socket still carries replies. The
 * client's local deadline marker is also a code, so it is excluded.
 */
function isDaemonAnswerError(error: unknown): boolean {
  return (
    error instanceof SubcError &&
    typeof error.code === "string" &&
    error.code !== REQUEST_DEADLINE_CODE &&
    !isSubcClientClosedError(error)
  );
}

/** Short `Class/kind/code` label for a drop log line. */
function describeFailure(error: unknown): string {
  if (!(error instanceof Error)) return `non-error:${String(error)}`;
  const className =
    error.name && error.name !== "Error" ? error.name : (error.constructor?.name ?? "Error");
  const parts = [className];
  if (error instanceof SubcCallError) parts.push(error.kind);
  const code = (error as { code?: unknown }).code;
  if (typeof code === "string" && code.length > 0) parts.push(code);
  return parts.join("/");
}

/** Why the pool is replacing its shared client; rendered as one log line. */
interface ClientDropCause {
  readonly error: unknown;
  /** The operation that surfaced the error: `request`, `route.open`, `bg_events stream`, … */
  readonly trigger: string;
  readonly session?: string;
  readonly route?: string;
  readonly detail?: string;
}

function routeLabel(route: RouteHandle | null | undefined): string | undefined {
  return route ? `${route.channel}@${route.epoch}` : undefined;
}

/** The liveness probe saw no reply on the shared connection within its window. */
export class SubcLivenessProbeTimeoutError extends Error {
  readonly code = "liveness_probe_timeout" as const;
  constructor(windowMs: number) {
    super(`no reply to a channel-0 catalog.list probe within ${windowMs}ms`);
    this.name = "SubcLivenessProbeTimeoutError";
  }
}

/** A held-open event subscription — the slice of subc-client's Subscription we use. */
export interface SubcSubscriptionLike {
  /** Cancel the subscription (sends Cancel; idempotent); the provider unwinds with StreamEnd. */
  unsubscribe(): void;
  /** Resolves on provider StreamEnd or unsubscribe; rejects on Error / route GOODBYE / socket drop. */
  readonly closed: Promise<void>;
}

/**
 * The minimal slice of {@link SubcClient} this transport depends on. Declared
 * structurally so a test can inject a fake client through the pool's `connect`
 * seam without standing up a daemon; the real `SubcClient` satisfies it.
 */
export interface SubcClientLike {
  routeOpen(
    target: RouteTarget,
    identity: BindIdentity,
    opts?: RouteOpenOptions,
  ): Promise<RouteHandle>;
  request(route: RouteHandle, body: unknown, opts?: RequestOptions): Promise<unknown>;
  subscribe(
    route: RouteHandle,
    body: unknown,
    onEvent: (event: Uint8Array) => void,
  ): SubcSubscriptionLike;
  closeRouteChannel(route: RouteHandle, opts?: { drain?: boolean }): Promise<void>;
  /**
   * A channel-0 control round trip on the same connection. The pool uses it as a
   * liveness probe before treating a run of unanswered calls as a dead socket.
   * Optional so a test double can model a client that offers no probe.
   */
  catalogList?(moduleId?: string): Promise<unknown>;
  /** Cumulative frames discarded because their route epoch did not match the client's current handle. */
  readonly droppedIngressFrames?: number;
  close(): void;
}

/** The subc module id AFT registers under (matches the daemon manifest). */
const AFT_MODULE_ID = "aft";
const HOST_SCOPE_LOOKUP_TIMEOUT_MS = 2_000;
const HOST_SCOPE_RETRY_FLOOR_MS = 5_000;
const HOST_SCOPE_RETRY_CAP_MS = 300_000;

function isHostScopeSyncRefusal(code: string): boolean {
  // crates/aft/src/gh_shim_relay/mod.rs lists scope_not_synced and scope_changed as retryable.
  // A live scope freshly reported by core can also be scope_not_live until the daemon applies it.
  return code === "scope_not_live" || code === "scope_not_synced" || code === "scope_changed";
}

type HostScopeAnswer = { result: "scope"; scope: RouteScope } | { result: "no_scope" };
type HostScopeResult =
  | HostScopeAnswer
  | { result: "error"; code: string; afterMs: number }
  | { result: "timeout"; afterMs: number }
  | { result: "malformed" };

interface HostScopeDecision {
  scope?: RouteScope;
  reason: string;
}

type HostRouteChannel = "tool" | "bg_events" | "scope_probe";

interface HostScopeState {
  answer?: HostScopeAnswer;
  promoteAfterBind?: boolean;
  scopeBindPending?: boolean;
  bindPending?: Promise<void>;
  pending?: Promise<HostScopeDecision>;
  unscopedReason?: string;
  retryTimer?: ReturnType<typeof setTimeout>;
  lookupAbort?: AbortController;
  retryAt: number;
  retryMs: number;
}

/**
 * A run of consecutive per-call failures that got no answer (timeout / route
 * GOODBYE / refusal) on the SAME client raises the suspicion of a dead half-open
 * connection: local writes succeed, no response ever arrives. The run alone never
 * closes the client, because the calls may belong to different sessions that are
 * each merely slow. It starts a liveness probe on the same connection, and the
 * client is dropped only if that probe also goes unanswered. The counter resets
 * on any successful request. Tool-level errors never count: they return
 * `success:false`, they do not throw. (Audit B-#4.)
 */
const MAX_CONSECUTIVE_TRANSPORT_FAILURES = 3;

/**
 * How long the liveness probe waits for its reply. subc-client documents that
 * the daemon handles a connection's channel-0 frames in order and can hold one
 * route.open for its whole bind relay (about 12s in production), so the probe
 * may queue behind a legitimate bind. A shorter window would convict a busy but
 * healthy connection.
 */
const LIVENESS_PROBE_TIMEOUT_MS = 15_000;

/** Code subc-client puts on a raw request or route.open that hit its own deadline. */
const REQUEST_DEADLINE_CODE = "request_deadline";

/** Every transport reconnect waits at least one event-loop turn and caps repeated failures. */
const RECONNECT_RETRY_FLOOR_MS = 100;
const RECONNECT_RETRY_CAP_MS = 2_000;
/**
 * Deadline of a route request that passes no explicit timeout. It mirrors
 * subc-client's `DEFAULT_REQUEST_TIMEOUT_MS`, which the client applies to such a
 * request and does not export, so the reload wait and the request share one budget.
 */
const SUBC_DEFAULT_REQUEST_TIMEOUT_MS = 30_000;

/**
 * Longest a call waits out route.open refusals while the AFT module reloads,
 * however long the call's own timeout is. A restart drains the old module for at
 * most the daemon's 30 s drain ceiling (reported as `drain_timeout_ms` by
 * `ck module status aft`), then starts the new module (normally well under a
 * second or two); 45 s covers that worst case with margin. The ceiling exists
 * because a module that is genuinely down must reach the caller's fallback (for
 * example bash host fallback) promptly instead of after a long transport timeout.
 */
const ROUTE_OPEN_RELOAD_WAIT_CEILING_MS = 45_000;

/** Resolution of the timer a call races its route.open against when its deadline passes first. */
const CALL_DEADLINE_EXPIRED: unique symbol = Symbol("call deadline expired");

function reloadWaitExhaustedSuffix(callDeadlineMs: number): string {
  if (callDeadlineMs > ROUTE_OPEN_RELOAD_WAIT_CEILING_MS) {
    const ceilingSeconds = ROUTE_OPEN_RELOAD_WAIT_CEILING_MS / 1000;
    return ` The AFT daemon module did not return within the ${ceilingSeconds}s reload-wait ceiling.`;
  }
  const seconds = Math.round(callDeadlineMs / 100) / 10;
  return ` The AFT daemon module did not return within this call's ${seconds}s deadline.`;
}

/**
 * A bg subscription that stayed up at least this long before dropping is treated
 * as "stable", so its reconnect backoff resets to zero. A subscription that fails
 * faster than this is escalating-broken and must keep backing off toward the cap
 * instead of repeatedly resubscribing in a hot loop.
 */
const BG_STABLE_MS = 5_000;

function reconnectBackoffMs(attempt: number): number {
  return Math.min(RECONNECT_RETRY_FLOOR_MS * 2 ** Math.min(attempt, 6), RECONNECT_RETRY_CAP_MS);
}
const BG_LIFECYCLE_LOG_INTERVAL_MS = 60_000;
const BG_DISPATCH_PROBE_INTERVAL_MS = 60_000;

/**
 * Session fallback when a tool runtime carries no session id, mirroring the Rust
 * `DEFAULT_SESSION_ID` (`protocol.rs`). Keeps undo/checkpoint/bash namespacing
 * identical to the standalone path for session-less calls.
 */
const DEFAULT_SESSION_ID = "__default__";

/**
 * Commands the plugin issues via `send()` that have NO meaning over subc and must
 * never hit the wire. `configure` is the prime case: under subc the RouteBind IS
 * the configure (AFT reads local `.cortexkit` config and ignores wire tiers — see
 * the unified-config model), so a `send("configure", …)` is satisfied locally
 * with a synthetic success rather than a route call.
 */
const LOCALLY_SATISFIED_COMMANDS = new Set(["configure"]);

export interface SubcTransportPoolOptions {
  /** Absolute path to the subc connection file (user-tier `subc.connection_file`). */
  connectionFile: string;
  /** Harness identity carried in every BindIdentity ("opencode" | "pi" | …). */
  harness: string;
  /**
   * Explicit route consumer identity. Undefined preserves subc-client's environment-derived
   * production identity; null is used by hermetic tests that run inside another module.
   */
  consumerIdentity?: ConsumerIdentity | null;
  /** Resolve existing host scopes before binding tool routes; enabled by the OpenCode and Pi plugins. */
  hostScopes?: boolean;
  /** Handshake timeout forwarded to SubcClient.connect. */
  handshakeTimeoutMs?: number;
  /**
   * Connection factory seam. Defaults to the real `SubcClient.connect`. Tests
   * inject a fake to exercise route caching / Rd reconnect without a daemon.
   */
  connect?: (opts: {
    connectionFile: string;
    handshakeTimeoutMs?: number;
    onControlPush?: (push: ControlPush) => void;
  }) => Promise<SubcClientLike>;
  /**
   * Called when an idle bg-completion WAKE arrives for `(projectRoot, session)`
   * (a `{op:"bg_events"}` StreamData nudge), AND immediately after each
   * (re)subscribe (the durable-outbox replay trigger). The nudge carries NO
   * payload — the handler MUST force a DRAIN (bash_drain_completions) to fetch
   * the actual completions. When set, the transport opens a dedicated bg_events
   * subscription per session and drives its reconnect independently of tool
   * calls (so an idle agent whose socket drops is still resubscribed + drained).
   * Absent ⇒ no bg subscriptions are opened.
   */
  onBgEventsNudge?: (projectRoot: string, session: string) => void;
  /** Test seam: backoff sleeper for the bg resubscribe loop (default real timer). */
  bgBackoffSleep?: (ms: number) => Promise<void>;
  /** Test seam for the pooled delay before reopening a route after an unknown-channel reply. */
  routeRetrySleep?: (ms: number) => Promise<void>;
  /** Test-only polling interval for detecting frames silently discarded with a stale route epoch. */
  bgDispatchProbeIntervalMs?: number;
  /** Test seam: how long the shared-connection liveness probe waits for its reply. */
  livenessProbeTimeoutMs?: number;
  /** Optional lifecycle registry used for root tracking; omit it to retain legacy behavior. */
  lifecycleRegistry?: LifecycleRegistry;
  /** Configuration-shaped alias used by construction sites that group lifecycle seams. */
  lifecycle?: {
    registry?: LifecycleRegistry;
    reapingEnabled?: boolean;
    demandCheck?: (
      root: CanonicalRootPath,
      poolId: ConcretePoolId,
    ) => boolean | { readonly exists?: boolean };
    evictOuterFacade?: (root: CanonicalRootPath, generation: RootGeneration) => void;
    onEvent?: (event: LifecycleEvent) => void;
  };
  /** Fixed registration switch; it is immutable for the registration lifetime. */
  reapingEnabled?: boolean;
  /** Alias for lifecycleDemandCheck matching LifecycleRegistry's seam name. */
  demandCheck?: (
    root: CanonicalRootPath,
    poolId: ConcretePoolId,
  ) => boolean | { readonly exists?: boolean };
  /**
   * Synchronous demand seam for the synchronous AftTransportPool interface. A
   * false result prevents a facade, session, and lifecycle root from being made.
   * Async demand callers can use getBridgeForDemand below.
   */
  lifecycleDemandCheck?: (
    root: CanonicalRootPath,
    poolId: ConcretePoolId,
  ) => boolean | { readonly exists?: boolean };
  /**
   * Optional callback used by direct concrete-pool tests. The wrapper normally
   * supplies this callback when it binds the returned registration handle.
   */
  evictOuterFacade?: (root: CanonicalRootPath, generation: RootGeneration) => void;
  /** Optional structured event sink for tests and host metrics. */
  onLifecycleEvent?: (event: LifecycleEvent) => void;
  /** Nudge rejection events are separate from root lifecycle events. */
  onBgNudgeRejected?: (event: SubcBgNudgeRejectedEvent) => void;
  /** Optional nudge callback that receives complete generation provenance. */
  onBgEventsNudgeRef?: (ref: BgNudgeRef) => void;
  /**
   * A pre-created registration is useful when a wrapper owns registration order:
   * bind it only after the wrapper callback has been installed.
   */
  lifecycleRegistration?: LifecyclePoolRegistration;
}

/** Complete provenance captured when a bg_events subscription is installed. */
export interface BgNudgeRef {
  readonly canonicalRoot: CanonicalRootPath;
  readonly session: string;
  readonly concretePoolId: ConcretePoolId;
  readonly generation: RootGeneration;
}

export interface SubcBgNudgeRejectedEvent {
  readonly type: "subc_bg_nudge_rejected";
  readonly canonicalRoot: CanonicalRootPath;
  readonly session: string;
  readonly expectedGeneration: RootGeneration;
  readonly currentGeneration?: RootGeneration;
  readonly expectedConcretePoolId: ConcretePoolId;
  readonly currentConcretePoolId?: ConcretePoolId;
}

interface RootGenerationErrorFields {
  readonly canonicalRoot: CanonicalRootPath;
  readonly expectedGeneration: RootGeneration;
  readonly currentGeneration?: RootGeneration;
  readonly concretePoolId?: ConcretePoolId;
  readonly currentConcretePoolId?: ConcretePoolId;
}

/** Stable classification for a request that loses its root to coordinated reap. */
export class SubcRootReapedError extends Error implements RootGenerationErrorFields {
  readonly code = "root_reaped" as const;
  readonly name = "SubcRootReapedError";
  readonly canonicalRoot: CanonicalRootPath;
  readonly expectedGeneration: RootGeneration;
  readonly currentGeneration?: RootGeneration;
  readonly concretePoolId?: ConcretePoolId;
  readonly currentConcretePoolId?: ConcretePoolId;

  constructor(fields: RootGenerationErrorFields) {
    super(`subc root was reaped: ${fields.canonicalRoot}`);
    this.canonicalRoot = fields.canonicalRoot;
    this.expectedGeneration = fields.expectedGeneration;
    this.currentGeneration = fields.currentGeneration;
    this.concretePoolId = fields.concretePoolId;
    this.currentConcretePoolId = fields.currentConcretePoolId;
  }
}

/** Stable classification for an operation holding an older root generation. */
export class SubcRootGenerationExpiredError extends Error implements RootGenerationErrorFields {
  readonly code = "root_generation_expired" as const;
  readonly name = "SubcRootGenerationExpiredError";
  readonly canonicalRoot: CanonicalRootPath;
  readonly expectedGeneration: RootGeneration;
  readonly currentGeneration?: RootGeneration;
  readonly concretePoolId?: ConcretePoolId;
  readonly currentConcretePoolId?: ConcretePoolId;

  constructor(fields: RootGenerationErrorFields) {
    super(`subc root generation expired: ${fields.canonicalRoot}`);
    this.canonicalRoot = fields.canonicalRoot;
    this.expectedGeneration = fields.expectedGeneration;
    this.currentGeneration = fields.currentGeneration;
    this.concretePoolId = fields.concretePoolId;
    this.currentConcretePoolId = fields.currentConcretePoolId;
  }
}

/** A synchronous lifecycle demand check could not establish that the root exists. */
export class SubcRootDemandRequiredError extends Error {
  readonly code = "root_demand_required" as const;
  readonly canonicalRoot: CanonicalRootPath;

  constructor(root: CanonicalRootPath) {
    super(`positive live demand is required before creating subc root state: ${root}`);
    this.name = "SubcRootDemandRequiredError";
    this.canonicalRoot = root;
  }
}

declare const identityKeyBrand: unique symbol;
export type IdentityKey = string & { readonly [identityKeyBrand]: "IdentityKey" };

function identityKey(identity: BindIdentity): IdentityKey {
  return `${identity.project_root}\u0000${identity.harness}\u0000${identity.session}` as IdentityKey;
}

/**
 * One session's held-open bg_events subscription with its own reconnect driver.
 *
 * The reconnect loop is independent of tool calls, including provider StreamEnd:
 * the module emits StreamEnd when a route is replaced, so only `stop()` proves
 * that a clean end was requested by this consumer. Reopening the dedicated route
 * also forces an outbox drain, recovering completions queued while disconnected.
 *
 * Each subscribe call owns one route-scoped ingress listener in the shared client.
 * A reconnect retires that route in `finally` before it opens the next one, so a
 * restart cannot layer listeners from multiple subscription lifecycles.
 */
class BgSubscription {
  private stopped = false;
  /** The live subscription handle, read by stop() to wake the loop's `await closed`. */
  private current: SubcSubscriptionLike | null = null;
  /** Client carrying the live subscription route, counted when that client is dropped. */
  private attached: SubcClientLike | null = null;
  private route: RouteHandle | null = null;
  private readonly loop: Promise<void>;
  private readonly lifecycleLogState = new Map<
    string,
    { lastEmittedAt: number; suppressed: number }
  >();
  private nudgeReceiptLogState: { lastEmittedAt: number; count: number } | null = null;

  constructor(
    private readonly identity: BindIdentity,
    private readonly acquireClient: () => Promise<SubcClientLike>,
    private readonly dropClient: (client: SubcClientLike, cause: ClientDropCause) => void,
    private readonly openRoute: (client: SubcClientLike) => Promise<RouteHandle>,
    private readonly onNudge: (observedEvent: boolean) => void,
    private readonly sleep: ((ms: number) => Promise<void>) | undefined,
    private readonly canAttach: () => boolean,
    private readonly onRootAttachFailure: (error: unknown) => boolean,
    private readonly onDormant: () => void,
    private readonly dispatchProbeIntervalMs: number,
    readonly nudgeRef?: BgNudgeRef,
    private readonly isCurrent: () => boolean = () => true,
    /** Reports a reply seen on the client, which proves its socket still delivers. */
    private readonly noteClientAlive: (client: SubcClientLike) => void = () => undefined,
    /** Reports a call on the client that got no answer; enough in a row start a liveness probe. */
    private readonly noteUnansweredCall: (
      client: SubcClientLike,
      cause: ClientDropCause,
    ) => void = () => undefined,
    private readonly onRouteAttached: () => void = () => undefined,
  ) {
    this.loop = this.run();
  }

  /** The client whose route currently carries this subscription, if any. */
  get attachedClient(): SubcClientLike | null {
    return this.attached;
  }

  get attachedRoute(): RouteHandle | null {
    return this.route;
  }

  private cancelBackoff: (() => void) | null = null;

  async stop(): Promise<void> {
    this.stopped = true;
    this.cancelBackoff?.();
    // Wake a live `await sub.closed`; the loop's `finally` remains the sole
    // owner of closeRouteChannel, avoiding a double close from stop() + run().
    const sub = this.current;
    if (sub) {
      try {
        sub.unsubscribe();
      } catch {
        // best-effort; the socket may already be gone
      }
    }
    await this.loop.catch(() => undefined);
  }

  private info(kind: string, message: string): void {
    const now = Date.now();
    const state = this.lifecycleLogState.get(kind);
    if (state && now - state.lastEmittedAt < BG_LIFECYCLE_LOG_INTERVAL_MS) {
      state.suppressed += 1;
      return;
    }
    const suppressed = state?.suppressed ?? 0;
    this.lifecycleLogState.set(kind, { lastEmittedAt: now, suppressed: 0 });
    const suffix = suppressed > 0 ? ` suppressed=${suppressed}` : "";
    log(`subc bg_events: ${message}${suffix}`, { sessionId: this.identity.session });
  }

  private routeId(route: RouteHandle): string {
    return `${route.channel}@${route.epoch}`;
  }

  private recordNudgeReceipt(routeId: string): void {
    const now = Date.now();
    const state = this.nudgeReceiptLogState;
    if (state && now - state.lastEmittedAt < BG_LIFECYCLE_LOG_INTERVAL_MS) {
      state.count += 1;
      return;
    }
    const count = (state?.count ?? 0) + 1;
    this.nudgeReceiptLogState = { lastEmittedAt: now, count: 0 };
    log(`subc bg_events: nudge received channel=${routeId} count=${count}`, {
      sessionId: this.identity.session,
    });
  }

  private errorText(error: unknown): string {
    return error instanceof Error ? `${error.name}: ${error.message}` : String(error);
  }

  private startDispatchProbe(client: SubcClientLike, routeId: string): () => void {
    const initial = client.droppedIngressFrames;
    if (typeof initial !== "number") return () => undefined;

    const existing = dispatchProbes.get(client);
    if (existing) {
      existing.users += 1;
      return () => releaseDispatchProbe(client);
    }

    let previous = initial;
    const timer = setInterval(() => {
      const total = client.droppedIngressFrames;
      if (typeof total !== "number" || total <= previous) return;
      const delta = total - previous;
      previous = total;
      this.info(
        "dispatch-epoch-drop",
        `client ingress epoch drops scope=client observed_while_channel=${routeId} delta=${delta} total=${total}`,
      );
    }, this.dispatchProbeIntervalMs);
    dispatchProbeTimers += 1;
    timer.unref?.();
    dispatchProbes.set(client, { users: 1, timer });
    return () => releaseDispatchProbe(client);
  }

  private async run(): Promise<void> {
    let backoffAttempt = 0;
    let reconnectAttempt = 0;
    let reconnecting = false;

    const beginReconnect = (): void => {
      reconnecting = true;
      reconnectAttempt = reconnectAttempt === 0 ? 1 : reconnectAttempt + 1;
    };
    const giveUp = (reason: string): void => {
      this.info(
        "reconnect-gave-up",
        `reconnect gave-up attempt=${reconnectAttempt} reason=${reason}`,
      );
    };

    while (!this.stopped) {
      if (!this.isCurrent()) {
        if (reconnecting) giveUp("stale-session");
        return;
      }
      if (!this.canAttach()) {
        if (reconnecting) giveUp("root-dormant");
        this.onDormant();
        return;
      }
      if (reconnecting) {
        this.info("reconnect-attempt", `reconnect attempt=${reconnectAttempt}`);
      }

      let client: SubcClientLike;
      try {
        client = await this.acquireClient();
      } catch (err) {
        if (!reconnecting) beginReconnect();
        this.info(
          "reconnect-error",
          `reconnect error attempt=${reconnectAttempt} error=${this.errorText(err)}`,
        );
        await this.backoff(backoffAttempt++);
        if (reconnecting) reconnectAttempt += 1;
        continue;
      }
      if (this.stopped) {
        if (reconnecting) giveUp("stopped");
        return;
      }
      if (!this.isCurrent()) {
        if (reconnecting) giveUp("stale-session");
        return;
      }
      if (!this.canAttach()) {
        if (reconnecting) giveUp("root-dormant");
        this.onDormant();
        return;
      }

      let route: RouteHandle;
      try {
        // A dedicated route keeps bg_events independent of the tool route's
        // credit window while preserving the client's connection-bound handle.
        route = await this.openRoute(client);
      } catch (err) {
        if (this.isCurrent() && this.onRootAttachFailure(err)) {
          if (reconnecting) giveUp("root-dormant");
          this.onDormant();
          return;
        }
        if (isSocketDeathError(err)) {
          this.dropClient(client, {
            error: err,
            trigger: "bg_events route.open",
            session: this.identity.session,
          });
        } else if (isDaemonAnswerError(err)) {
          this.noteClientAlive(client);
        } else if (isUnansweredCallError(err)) {
          this.noteUnansweredCall(client, {
            error: err,
            trigger: "bg_events route.open",
            session: this.identity.session,
          });
        }
        if (!reconnecting) beginReconnect();
        this.info(
          "reconnect-error",
          `reconnect error attempt=${reconnectAttempt} error=${this.errorText(err)}`,
        );
        await this.backoff(backoffAttempt++);
        if (reconnecting) reconnectAttempt += 1;
        continue;
      }
      if (this.stopped || !this.isCurrent()) {
        safeCloseRoute(client, route);
        if (reconnecting) giveUp(this.stopped ? "stopped" : "stale-session");
        return;
      }
      this.noteClientAlive(client);

      const subscribedAt = Date.now();
      const routeId = this.routeId(route);
      let stopDispatchProbe = (): void => undefined;
      try {
        const sub = client.subscribe(route, { op: "bg_events" }, () => {
          if (this.stopped) {
            this.info(
              "nudge-drop-stopped",
              `nudge dropped cause=subscription-stopped channel=${routeId}`,
            );
            return;
          }
          this.recordNudgeReceipt(routeId);
          this.noteClientAlive(client);
          if (!this.isCurrent()) {
            this.info(
              "nudge-stale-carrier",
              `nudge carried by stale subscription; checking current session channel=${routeId}`,
            );
          }
          this.onNudge(true);
        });
        this.current = sub;
        this.attached = client;
        this.route = route;
        this.onRouteAttached();
        stopDispatchProbe = this.startDispatchProbe(client, routeId);
        this.info("subscription-open", `subscription open channel=${routeId}`);
        if (reconnecting) {
          this.info(
            "reconnect-success",
            `reconnect success attempt=${reconnectAttempt} channel=${routeId}`,
          );
          reconnecting = false;
          reconnectAttempt = 0;
        }

        // stop() may have fired between routeOpen and subscribe. Unsubscribe so
        // the held `closed` promise resolves and the route can unwind.
        if (this.stopped) sub.unsubscribe();

        // A reconnect may have missed completions, so force a drain after every
        // successful subscribe to replay anything queued while the stream was down.
        if (!this.stopped && this.isCurrent()) this.onNudge(false);

        await sub.closed;
        this.info("stream-end", `stream ended channel=${routeId}`);
        if (this.stopped) {
          giveUp("stopped");
          return;
        }
        // Provider StreamEnd is also emitted for route replacement; without an
        // explicit local stop it must reopen rather than strand an idle session.
        if (Date.now() - subscribedAt >= BG_STABLE_MS) backoffAttempt = 0;
        beginReconnect();
      } catch (err) {
        const routeId = this.routeId(route);
        this.info("stream-error", `stream error channel=${routeId} error=${this.errorText(err)}`);
        if (this.stopped) {
          giveUp("stopped");
          return;
        }
        if (isSocketDeathError(err)) {
          this.dropClient(client, {
            error: err,
            trigger: "bg_events stream",
            session: this.identity.session,
            route: routeId,
          });
        }
        if (Date.now() - subscribedAt >= BG_STABLE_MS) backoffAttempt = 0;
        beginReconnect();
      } finally {
        stopDispatchProbe();
        this.current = null;
        this.attached = null;
        this.route = null;
        safeCloseRoute(client, route);
      }
      await this.backoff(backoffAttempt++);
    }

    if (reconnecting) giveUp("stopped");
  }

  private async backoff(attempt: number): Promise<void> {
    if (this.stopped) return;
    await new Promise<void>((resolve, reject) => {
      let timer: ReturnType<typeof setTimeout> | undefined;
      let finished = false;
      const settle = (error?: unknown): void => {
        if (finished) return;
        finished = true;
        if (timer) clearTimeout(timer);
        this.cancelBackoff = null;
        if (error !== undefined) reject(error);
        else resolve();
      };
      this.cancelBackoff = () => settle();
      const delay = reconnectBackoffMs(attempt);
      if (this.sleep) {
        // An injected sleeper may not be cancellable. Observe its eventual
        // rejection, but let stop release the subscription without awaiting it.
        try {
          void this.sleep(delay).then(() => settle(), settle);
        } catch (error) {
          settle(error);
        }
      } else {
        timer = setTimeout(() => settle(), delay);
      }
    });
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isRouteProvenAbsentError(err: unknown): boolean {
  return (
    (err instanceof SubcError &&
      (err.code === "unknown_channel" || err.code === "stale_route_epoch")) ||
    err instanceof StaleRouteHandleError
  );
}

/**
 * A daemon can reject a route bind after the root has been reclaimed. This
 * signature is deliberately narrower than config_divergence: other divergence
 * causes remain transient from the transport's point of view.
 */
function isAbsentRootRouteError(err: unknown): boolean {
  return (
    isRecord(err) &&
    err.code === "config_divergence" &&
    typeof err.message === "string" &&
    err.message.includes("project root does not exist")
  );
}

function absentRootError(root: CanonicalRootPath): SubcError {
  return new SubcError(
    `invalid route project root: project root does not exist: ${root}`,
    "config_divergence",
  );
}

/** Preserve the daemon's final refusal while making the reload timeout actionable. */
function reloadWindowExhaustedError(error: unknown, callDeadlineMs: number): unknown {
  return withDeadlineSuffix(error, reloadWaitExhaustedSuffix(callDeadlineMs));
}

/**
 * The error a call surfaces once its own deadline has passed before a reply.
 * `lastError` is the most recent route refusal the call saw (it keeps its code,
 * so a reload refusal still lets bash take its host fallback); with none, the
 * error carries subc-client's own `request_deadline` code, the same one a
 * route.open or request that ran out of time inside the client would carry.
 */
function callDeadlinePassedError(lastError: unknown, callDeadlineMs: number): unknown {
  const seconds = Math.round(callDeadlineMs / 100) / 10;
  const suffix = ` The AFT daemon module did not return within this call's ${seconds}s deadline.`;
  if (lastError !== undefined) return withDeadlineSuffix(lastError, suffix);
  return new SubcError(
    `route to module '${AFT_MODULE_ID}' was not ready before the call ran out of time.${suffix}`,
    REQUEST_DEADLINE_CODE,
  );
}

export const __withDeadlineSuffixForTests = withDeadlineSuffix;

function withDeadlineSuffix(error: unknown, suffix: string): unknown {
  if (!(error instanceof Error)) return new Error(`${String(error)}${suffix}`, { cause: error });
  const message = `${error.message}${suffix}`;
  if (error instanceof SubcCallError) {
    const wrapped = new SubcCallError(error.kind, message, error.code, error);
    // SubcCallError's detail getter only unwraps a direct SubcError cause.
    Object.defineProperty(wrapped, "detail", { value: error.detail });
    return wrapped;
  }
  if (error instanceof SubcError) {
    const wrapped = new SubcError(message, error.code, error.detail, error.closeReason);
    Object.defineProperty(wrapped, "cause", { value: error });
    return wrapped;
  }
  const wrapped = new Error(message, { cause: error });
  wrapped.name = error.name;
  return wrapped;
}

/**
 * Fire-and-forget route close that can never throw — neither synchronously (a
 * client that rejects/throws when closing a route on an already-dead socket) nor
 * via an unhandled rejection. Used on every best-effort teardown path.
 */
function safeCloseRoute(client: SubcClientLike, route: RouteHandle): void {
  try {
    void client.closeRouteChannel(route).catch(() => undefined);
  } catch {
    // synchronous throw (e.g. closing a route on an already-closed client) — ignore
  }
}

function abortedRequestError(): Error {
  const error = new Error("AFT request aborted by host cancellation");
  error.name = "AbortError";
  return error;
}

// The metric is connection-wide, so observing it needs one timer, not one per
// session. Last unsubscribe releases the timer, including on a dead socket.
const dispatchProbes = new Map<
  SubcClientLike,
  { users: number; timer: ReturnType<typeof setInterval> }
>();
let dispatchProbeTimers = 0;
function releaseDispatchProbe(client: SubcClientLike): void {
  const probe = dispatchProbes.get(client);
  if (!probe || --probe.users > 0) return;
  clearInterval(probe.timer);
  dispatchProbeTimers -= 1;
  dispatchProbes.delete(client);
}
export function __dispatchProbeCountForTests(): number {
  return dispatchProbeTimers;
}

/** Per-identity session lifecycle state, independent from transient route churn. */
interface SessionRecord {
  /** Complete identity is retained; the opaque map key is never parsed. */
  readonly identity: BindIdentity;
  readonly identityKey: IdentityKey;
  readonly canonicalRoot: CanonicalRootPath;
  readonly generation?: RootGeneration;
  /** Current tool route for this session incarnation; replaced after route failures. */
  routeEntry: RouteEntry | null;
  /** Replaced carriers still owned by pending unary requests, including cancelled callers. */
  readonly retiredRoutes: Set<RouteEntry>;
  /** Dedicated bg_events subscription, present only when background events are enabled. */
  bgSub: BgSubscription | null;
  /** Closed marker set synchronously so in-flight requests can see the close. */
  closed: boolean;
  /** Root close marks this before any asynchronous cleanup starts. */
  teardownReason: "root_reaped" | "session_closed" | "shutdown" | null;
  /** Count of in-flight requests on this session's route; used for safe cleanup. */
  inflight: number;
  lastUsedAt: number;
  observedFromStart: boolean;
  readonly backgroundTasks: Map<string, "running" | "terminal" | "unknown">;
  readonly backgroundWatches: Map<string, string>;
  unknownBackgroundWork: boolean;
  pendingBgNudge: boolean;
  pendingDetach: boolean;
}

interface DetachedSession {
  readonly record: SessionRecord;
  readonly bgSub: BgSubscription | null;
  readonly routeEntry: RouteEntry | null;
}

/**
 * Per-identity tool-route state. Installed BEFORE the `routeOpen` await so two
 * concurrent first calls for the same identity share ONE open (singleflight)
 * instead of each minting a channel and leaking the loser (audit B-#3). `closed`
 * tombstones the entry when `closeSession`/`shutdown`/`dropClient` races an
 * in-flight open: the resolving open observes it and closes the just-opened
 * channel instead of caching a stale route.
 */
interface RouteEntry {
  /** Scope used to bind this tool carrier; absent on a fallback route. */
  hostScope?: RouteScope;
  /** Route provenance prevents late work from touching a successor generation. */
  readonly canonicalRoot: CanonicalRootPath;
  readonly generation?: RootGeneration;
  /** Client that minted this route; closeSession must close channels on this owner. */
  client: SubcClientLike;
  /** In-flight routeOpen; non-null until it settles. Concurrent callers await it. */
  opening: Promise<RouteHandle> | null;
  /** Exact connection-bound handle once open; null while still opening. */
  handle: RouteHandle | null;
  /** Tombstone: a teardown raced the open — the resolving open must self-close. */
  closed: boolean;
  /** Pending client requests, not callers: host cancellation can finish a caller first. */
  inflight: number;
  /**
   * Retirement reason. Replacement stops new dispatches but closes the carrier
   * only after all pending requests settle. Explicit teardown can still close
   * it immediately, and the session's teardown reason takes precedence then.
   */
  closedBy: SubcLocalRouteCloseReason | null;
}

/**
 * A route open that resolved AFTER its session was torn down (closeSession /
 * shutdown / client swap). The caller's request can't proceed, but this is an
 * intentional teardown — NOT a transport fault — so it must not drop the client
 * or count toward the half-open-socket failure budget (B-#3/B-#4).
 */
class RouteTornDownError extends Error {}

/**
 * True when a call stopped at a teardown before its request was handed to the
 * client (a closed session, or a route that opened after the teardown), so the
 * request provably never reached AFT.
 */
export function isSubcRouteTornDownError(error: unknown): boolean {
  return error instanceof RouteTornDownError;
}

/**
 * Turn subc-client's bare rejection of a request that was in flight on a route
 * the plugin closed into an outcome-unknown error that names the close reason.
 * Any other error passes through unchanged.
 */
function routeClosedMidCallError(
  error: unknown,
  entry: RouteEntry,
  record: SessionRecord,
): unknown {
  if (!isLocalRouteCloseRejection(error)) return error;
  const reason =
    teardownCloseReason(record.teardownReason) ?? entry.closedBy ?? "route closed by the plugin";
  return new SubcRouteClosedMidCallError(reason, error);
}

/**
 * Re-lift the route reply into the flat {@link ToolCallResult} shape the standalone
 * `BinaryBridge.toolCall` returns. The Rust module wraps the full flat response
 * (`{id, success, …data, text}`) under `structuredContent` (S1 envelope), alongside
 * the MCP `{content, isError}` a generic host reads. The first-party plugin reads
 * `structuredContent`, so re-lifting it makes everything downstream (status_bar,
 * bg_completions, preview_diff, code, …) byte-identical to NDJSON.
 *
 * Every AFT tool reply over subc carries this envelope with a boolean `success`.
 * During the wire transition, `structuredContent.text` may be omitted; the bridge
 * synthesizes it only from exactly one outer text block, while a present field wins
 * unchanged. A reply missing the envelope, lacking boolean `success`, or missing
 * text without an unambiguous outer block is a PROTOCOL VIOLATION — never a tool
 * result — and is thrown rather than coerced. Coercing it (the old
 * `{success:false,text:""}` / raw-record fallback) could let a malformed reply with
 * `success === undefined` read downstream as a successful tool result (audit B-#7).
 * Surfacing it loudly is the honest contract: a broken wire shape is a failure, not
 * a silent empty pass.
 */
function reliftReply(reply: unknown): Record<string, unknown> {
  if (!isRecord(reply) || !isRecord(reply.structuredContent)) {
    throw new Error(
      "subc tool reply is missing the structuredContent envelope (protocol violation)",
    );
  }
  const flat = reply.structuredContent;
  if (typeof flat.success !== "boolean") {
    throw new Error(
      "subc tool reply structuredContent lacks a boolean `success` / string `text` (protocol violation)",
    );
  }
  if (!Object.hasOwn(flat, "text")) {
    const content = reply.content;
    const block = Array.isArray(content) && content.length === 1 ? content[0] : undefined;
    if (isRecord(block) && block.type === "text" && typeof block.text === "string") {
      return { ...flat, text: block.text };
    }
  }
  if (typeof flat.text !== "string") {
    throw new Error(
      "subc tool reply structuredContent lacks a boolean `success` / string `text` (protocol violation)",
    );
  }
  return flat;
}

/**
 * One project root's view onto the shared subc client. Holds per-root status
 * caches (mirroring BinaryBridge) and routes every call through the pool's single
 * client, opening+caching a route per `(root, harness, session)`.
 */
class SubcTransport implements AftProjectTransport {
  private cachedStatus: StatusSnapshot | null = null;

  constructor(
    private readonly pool: SubcTransportPool,
    private readonly projectRoot: CanonicalRootPath,
    private readonly generation?: RootGeneration,
  ) {}

  getCwd(): string {
    return this.projectRoot;
  }

  /** Generation provenance is intentionally observable for lifecycle tests and nudge wiring. */
  getGeneration(): RootGeneration | undefined {
    return this.generation;
  }

  getConcretePoolId(): ConcretePoolId | undefined {
    return this.pool.getConcretePoolId();
  }

  getCachedStatus(): StatusSnapshot | null {
    return this.cachedStatus;
  }

  cacheStatusSnapshot(snapshot: StatusSnapshot): void {
    this.cachedStatus = snapshot;
  }

  private identityFor(session: string | undefined): BindIdentity {
    return {
      project_root: this.projectRoot,
      harness: this.pool.harness,
      session: session && session.length > 0 ? session : DEFAULT_SESSION_ID,
    };
  }

  private assertCurrent(): void {
    this.pool.assertFacadeCurrent(this.projectRoot, this.generation, "facade_use");
  }

  async toolCall(
    sessionId: string | undefined,
    name: string,
    rawArgs: ToolCallArguments = {},
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    this.assertCurrent();
    const { preview, timeoutMs, onProgress, abortSignal } = this.splitOptions(options);
    const body: Record<string, unknown> = { name, arguments: rawArgs };
    if (onProgress) body.handles_progress = true;
    const editSlotSurvives = this.pool.getEditSlotSurvives();
    if (editSlotSurvives !== undefined) body.edit_slot_survives = editSlotSurvives;
    if (preview === true) body.preview = true;
    if (options?.workerSession === true) body[WORKER_SESSION_FIELD] = true;
    if (options?.hostToolNames) body[HOST_TOOL_NAMES_FIELD] = options.hostToolNames;
    body[PRESET_FIELD] = callPresetFor(options?.workerSession === true);
    const reply = await this.pool.routeRequest(
      this.identityFor(sessionId),
      body,
      timeoutMs,
      onProgress,
      this.generation,
      abortSignal,
    );
    return reliftReply(reply) as ToolCallResult;
  }

  /**
   * Lifecycle / native-command path. Over subc there is no separate "native
   * command" channel — every command rides the tool_provider route as a
   * `{name, arguments}` Request and the module's gate decides validity (the 21
   * core tools plus the `bash_drain_completions` / `bash_ack_completions` plumbing
   * allowlist). The bind session is taken from `params.session_id` so a
   * session-scoped command (drain/ack) reaches the matching route — the module
   * re-injects the BIND session over any body session, so the route identity is
   * what scopes it. `configure` is satisfied locally (binding is the configure).
   */
  async send(
    command: string,
    params: Record<string, unknown> = {},
    options?: AftTransportOptions,
  ): Promise<Record<string, unknown>> {
    this.assertCurrent();
    if (LOCALLY_SATISFIED_COMMANDS.has(command)) {
      return { success: true, command, subc_local: true };
    }
    const { timeoutMs, onProgress, abortSignal } = this.splitOptions(options);
    const session = typeof params.session_id === "string" ? params.session_id : undefined;
    // The caller's role travels beside the call, like the session (which the
    // route bind carries), not inside the command's arguments: the module
    // reads it from the call body.
    // The caller's tool names travel the same way: RouteBind has no field for
    // them, and one route's host must not decide another route's wording.
    const {
      [WORKER_SESSION_FIELD]: workerSession,
      [HOST_TOOL_NAMES_FIELD]: hostToolNames,
      ...args
    } = params;
    const body: Record<string, unknown> = { name: command, arguments: args };
    if (onProgress) body.handles_progress = true;
    if (workerSession === true) body[WORKER_SESSION_FIELD] = true;
    if (hostToolNames !== undefined) body[HOST_TOOL_NAMES_FIELD] = hostToolNames;
    body[PRESET_FIELD] = callPresetFor(workerSession === true);
    const editSlotSurvives = this.pool.getEditSlotSurvives();
    if (editSlotSurvives !== undefined) body.edit_slot_survives = editSlotSurvives;
    const reply = await this.pool.routeRequest(
      this.identityFor(session),
      body,
      timeoutMs,
      onProgress,
      this.generation,
      abortSignal,
    );
    return reliftReply(reply);
  }

  private splitOptions(options?: ToolCallOptions): {
    preview?: boolean;
    timeoutMs?: number;
    onProgress?: RequestOptions["onProgress"];
    abortSignal?: AbortSignal;
  } {
    if (!options) return {};
    const preview = (options as ToolCallOptions).preview;
    // Mirror BinaryBridge.send's budget precedence (transportTimeoutMs wins):
    // orchestrated bash passes its wait-aware budget as transportTimeoutMs, and
    // dropping it here would cap long tool executions at the subc client's
    // default unary deadline while the command keeps running module-side.
    const timeoutMs = options.transportTimeoutMs ?? options.timeoutMs;
    // subc-client delivers wire bodies; hosts use the same decoded callback
    // shape as BinaryBridge, regardless of the route carrying the request.
    const callback = options.onProgress;
    const onProgress: RequestOptions["onProgress"] = callback
      ? (body) => {
          let frame: unknown;
          try {
            frame = JSON.parse(new TextDecoder().decode(body));
          } catch {
            return;
          }
          if (typeof frame !== "object" || frame === null) return;
          const progress = frame as Record<string, unknown>;
          if (progress.type !== "progress" || typeof progress.chunk !== "string") return;
          callback({
            kind: progress.kind === "stderr" ? "stderr" : "stdout",
            text: progress.chunk,
          });
        }
      : undefined;
    return { preview, timeoutMs, onProgress, abortSignal: options.abortSignal };
  }
}

/**
 * Route cache over one authenticated subc client. In lifecycle mode this class
 * also owns the concrete pool's root index; the registry owns timer and root
 * transition state, while this class owns sessions, routes, and subscriptions.
 */
export class SubcTransportPool implements AftTransportPool {
  readonly harness: string;
  private readonly connectionFile: string;
  private readonly handshakeTimeoutMs?: number;
  private readonly consumerIdentity: ConsumerIdentity | null | undefined;
  private readonly hostScopes: boolean;
  /** Scope identity is host-session-wide, even when the session uses multiple project roots. */
  private readonly hostScopeCache = new Map<string, HostScopeState>();
  private readonly hostScopeRefusals = new Map<string, string>();
  private readonly hostScopeOutcomes = new Map<string, string>();
  private readonly connectFn: (opts: {
    connectionFile: string;
    handshakeTimeoutMs?: number;
    onControlPush?: (push: ControlPush) => void;
  }) => Promise<SubcClientLike>;

  private readonly onBgEventsNudge?: (projectRoot: string, session: string) => void;
  private readonly onBgEventsNudgeRef?: (ref: BgNudgeRef) => void;
  private readonly bgBackoffSleep: ((ms: number) => Promise<void>) | undefined;
  private readonly routeRetrySleep: (ms: number) => Promise<void>;
  private readonly bgDispatchProbeIntervalMs: number;
  private readonly livenessProbeTimeoutMs: number;
  private readonly lifecycleDemandCheck?: (
    root: CanonicalRootPath,
    poolId: ConcretePoolId,
  ) => boolean | { readonly exists?: boolean };
  private readonly onLifecycleEvent?: (event: LifecycleEvent) => void;
  private readonly onBgNudgeRejected?: (event: SubcBgNudgeRejectedEvent) => void;

  private lifecycleRegistry?: LifecycleRegistry;
  private registryUsesPoolEventSink = false;
  private lifecycleRegistration: LifecyclePoolRegistration | null = null;
  private outerFacadeEvictor: (root: CanonicalRootPath, generation: RootGeneration) => void;

  private client: SubcClientLike | null = null;
  private hadWorkingClient = false;
  /** Single-flight guard so concurrent first calls share one connect. */
  private connecting: Promise<SubcClientLike> | null = null;
  /** The growing delay for a safe, once-only route resend after route closure. */
  private routeReopenRetryDelayMs = RECONNECT_RETRY_FLOOR_MS;
  /** Concurrent route closures and route.open refusals wait for the same retry timer. */
  private routeReopenRetry: Promise<void> | null = null;
  /** Delay assigned to the shared retry timer while it is pending. */
  private routeReopenRetryMs: number | null = null;
  /** Per-session records keyed by the opaque identity key. */
  private readonly sessions = new Map<IdentityKey, SessionRecord>();
  /** Keep global insertion order so a session using several roots selects the same first live route. */
  private readonly sessionIndex = new Map<string, Set<SessionRecord>>();
  // Pending host creation events are bounded. Forgetting one only makes that
  // session unknown (retained), never falsely eligible for eviction.
  private readonly observedStarts = new Set<IdentityKey>();
  private sessionSweep: ReturnType<typeof setInterval> | null = null;
  /**
   * The sole root-scoped session enumeration authority. Keys are opaque and are
   * removed by the same detacher that removes the corresponding session record.
   */
  private readonly rootIndex = new Map<CanonicalRootPath, Set<IdentityKey>>();
  /** Roots whose route binds must stay dormant until their directories return. */
  private readonly dormantRoots = new Map<CanonicalRootPath, SubcError>();
  /** Consecutive unanswered calls on the current pool-local client. */
  private transportFailures = 0;
  /** The in-flight liveness probe of the current client; at most one runs at a time. */
  private livenessProbe: Promise<void> | null = null;
  /**
   * Bumped whenever the current client delivers any reply. A liveness probe that
   * times out while this moved has been outrun by other traffic, not by a dead socket.
   */
  private clientLivenessMark = 0;
  /** Concrete per-root facades, including their captured root generation. */
  private readonly transports = new Map<CanonicalRootPath, SubcTransport>();
  private readonly generationRejections = new Set<string>();
  private readonly nudgeDeliveryLogState = new Map<
    IdentityKey,
    Map<string, { lastEmittedAt: number; suppressed: number }>
  >();
  private readonly pendingRootCleanups = new Set<Promise<unknown>>();
  private shuttingDown = false;
  private editSlotSurvives: boolean | undefined;
  private editSlotSurvivesCaptured = false;

  constructor(options: SubcTransportPoolOptions) {
    this.connectionFile = options.connectionFile;
    this.harness = options.harness;
    this.handshakeTimeoutMs = options.handshakeTimeoutMs;
    this.consumerIdentity = options.consumerIdentity;
    this.hostScopes = options.hostScopes ?? false;
    this.connectFn = options.connect ?? ((opts) => SubcClient.connect(opts));
    this.onBgEventsNudge = options.onBgEventsNudge;
    this.onBgEventsNudgeRef = options.onBgEventsNudgeRef;
    this.bgBackoffSleep = options.bgBackoffSleep;
    this.routeRetrySleep =
      options.routeRetrySleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms)));
    this.bgDispatchProbeIntervalMs =
      options.bgDispatchProbeIntervalMs ?? BG_DISPATCH_PROBE_INTERVAL_MS;
    this.livenessProbeTimeoutMs = options.livenessProbeTimeoutMs ?? LIVENESS_PROBE_TIMEOUT_MS;
    const lifecycle = options.lifecycle;
    const demandCheck =
      options.lifecycleDemandCheck ?? options.demandCheck ?? lifecycle?.demandCheck;
    this.lifecycleDemandCheck = demandCheck;
    this.onLifecycleEvent = options.onLifecycleEvent ?? lifecycle?.onEvent;
    this.onBgNudgeRejected = options.onBgNudgeRejected;
    this.outerFacadeEvictor =
      options.evictOuterFacade ?? lifecycle?.evictOuterFacade ?? (() => undefined);

    const lifecycleRegistry = options.lifecycleRegistry ?? lifecycle?.registry;
    if (lifecycleRegistry || demandCheck || options.lifecycleRegistration) {
      this.registryUsesPoolEventSink =
        lifecycleRegistry === undefined && this.onLifecycleEvent !== undefined;
      this.lifecycleRegistry =
        lifecycleRegistry ??
        new LifecycleRegistry({
          demandCheck: demandCheck ? (root, poolId) => demandCheck(root, poolId) : undefined,
          onEvent: this.registryUsesPoolEventSink ? this.onLifecycleEvent : undefined,
        });
      if (options.lifecycleRegistration) {
        this.bindLifecycleRegistration(options.lifecycleRegistration);
      } else {
        this.bindLifecycleRegistration(
          this.lifecycleRegistry.registerLifecyclePool(this, {
            reapingEnabled: options.reapingEnabled ?? lifecycle?.reapingEnabled ?? false,
            evictOuterFacade: (root, generation) => this.outerFacadeEvictor(root, generation),
          }),
        );
      }
    }
  }

  /**
   * Bind a wrapper-owned registration after its generation-matched eviction
   * callback has been installed. The handle is the only authority that may
   * deregister this concrete pool.
   */
  bindLifecycleRegistration(registration: LifecyclePoolRegistration): void {
    if (this.lifecycleRegistration && this.lifecycleRegistration !== registration) {
      this.lifecycleRegistration.deregister();
    }
    this.lifecycleRegistration = registration;
  }

  /** Construction helper for wrappers that own the registration sequence. */
  registerLifecyclePool(
    registry: LifecycleRegistry,
    options: LifecyclePoolRegistrationOptions,
  ): LifecyclePoolRegistration {
    this.lifecycleRegistry = registry;
    this.outerFacadeEvictor = options.evictOuterFacade;
    const registration = registry.registerLifecyclePool(this, options);
    this.bindLifecycleRegistration(registration);
    return registration;
  }

  /** Replace only the callback; registration identity and switch stay immutable. */
  setOuterFacadeEvictor(
    evictOuterFacade: (root: CanonicalRootPath, generation: RootGeneration) => void,
  ): void {
    this.outerFacadeEvictor = evictOuterFacade;
  }

  getConcretePoolId(): ConcretePoolId | undefined {
    return this.lifecycleRegistration?.concretePoolId;
  }

  getCurrentRootGeneration(root: CanonicalRootPath): RootGeneration | undefined {
    return this.currentGeneration(root);
  }

  recordBgNudgeRejection(ref: BgNudgeRef): void {
    const event: SubcBgNudgeRejectedEvent = {
      type: "subc_bg_nudge_rejected",
      canonicalRoot: ref.canonicalRoot,
      session: ref.session,
      expectedGeneration: ref.generation,
      currentGeneration: this.currentGeneration(ref.canonicalRoot),
      expectedConcretePoolId: ref.concretePoolId,
      currentConcretePoolId: this.currentPoolId(),
    };
    this.onBgNudgeRejected?.(event);
  }

  getLifecycleRegistration(): LifecyclePoolRegistration | null {
    return this.lifecycleRegistration;
  }

  getLifecycleRegistry(): LifecycleRegistry | undefined {
    return this.lifecycleRegistry;
  }

  static async connectionAvailable(connectionFile: string): Promise<boolean> {
    return connectionFileExists(connectionFile);
  }

  private canonicalRoot(projectRoot: string): CanonicalRootPath {
    return asCanonicalRootPath(canonicalizeProjectRoot(projectRoot));
  }

  /**
   * Check a root immediately before opening a route. Only the directory's
   * existence decides: a reclaim marker beside it (`<root>.reclaimed`) needs
   * no check, since an absent directory suspends the bind either way and a
   * directory that has returned wins over a stale marker.
   */
  private rootCanAttach(root: CanonicalRootPath): boolean {
    let directoryExists = false;
    try {
      directoryExists = statSync(root).isDirectory();
    } catch {
      // Missing, inaccessible, or otherwise unusable roots cannot be bound.
    }
    if (directoryExists) {
      this.dormantRoots.delete(root);
      return true;
    }
    this.markRootDormant(root);
    return false;
  }

  private markRootDormant(root: CanonicalRootPath, rejection?: unknown): SubcError {
    const existing = this.dormantRoots.get(root);
    if (existing) return existing;
    const error = rejection instanceof SubcError ? rejection : absentRootError(root);
    this.dormantRoots.set(root, error);
    log(`root ${root} reclaimed/absent; suspending attach until it exists`);
    return error;
  }

  private assertRootCanAttach(root: CanonicalRootPath): void {
    if (this.rootCanAttach(root)) return;
    throw this.dormantRoots.get(root) ?? absentRootError(root);
  }

  private lifecycleEnabled(): boolean {
    // A disabled registration deliberately behaves like the pre-reaper pool:
    // synthetic roots may be used, no demand check is required, and no sweep can
    // acquire a tombstone for the pool.
    return Boolean(this.lifecycleRegistry && this.lifecycleRegistration?.reapingEnabled);
  }

  private currentGeneration(root: CanonicalRootPath): RootGeneration | undefined {
    const registration = this.lifecycleRegistration;
    return registration && this.lifecycleRegistry
      ? this.lifecycleRegistry.currentGeneration(registration.concretePoolId, root)
      : undefined;
  }

  private currentPoolId(): ConcretePoolId | undefined {
    return this.lifecycleRegistration?.concretePoolId;
  }

  private isCurrentLiveGeneration(
    root: CanonicalRootPath,
    generation: RootGeneration | undefined,
  ): boolean {
    if (!this.lifecycleEnabled()) return true;
    const registry = this.lifecycleRegistry;
    const poolId = this.currentPoolId();
    return (
      registry !== undefined &&
      poolId !== undefined &&
      generation !== undefined &&
      registry.isCurrentLiveGeneration(poolId, root, generation)
    );
  }

  private isRootTombstoned(root: CanonicalRootPath, generation: RootGeneration): boolean {
    const registry = this.lifecycleRegistry;
    const poolId = this.currentPoolId();
    return (
      registry !== undefined &&
      poolId !== undefined &&
      registry.isTombstoned(poolId, root, generation)
    );
  }

  private recordGenerationRejection(
    root: CanonicalRootPath,
    expectedGeneration: RootGeneration,
    boundary: string,
  ): void {
    if (!this.lifecycleEnabled()) return;
    const poolId = this.currentPoolId();
    const registry = this.lifecycleRegistry;
    if (!poolId || !registry) return;
    const key = `${poolId}\u0000${root}\u0000${expectedGeneration}\u0000${boundary}`;
    if (this.generationRejections.has(key)) return;
    this.generationRejections.add(key);
    registry.recordGenerationRejection(poolId, root, expectedGeneration, boundary);
    if (this.registryUsesPoolEventSink) return;
    this.onLifecycleEvent?.({
      type: "subc_root_generation_rejected",
      realm: registry.realm,
      concretePoolId: poolId,
      canonicalRoot: root,
      expectedGeneration,
      currentGeneration: registry.currentGeneration(poolId, root),
      boundary,
    });
  }

  private generationExpiredError(
    root: CanonicalRootPath,
    expectedGeneration: RootGeneration,
  ): SubcRootGenerationExpiredError {
    const currentGeneration = this.currentGeneration(root);
    this.recordGenerationRejection(root, expectedGeneration, "stale_generation");
    return new SubcRootGenerationExpiredError({
      canonicalRoot: root,
      expectedGeneration,
      currentGeneration,
      concretePoolId: this.currentPoolId(),
      currentConcretePoolId: this.currentPoolId(),
    });
  }

  private rootReapedError(record: SessionRecord): SubcRootReapedError {
    return new SubcRootReapedError({
      canonicalRoot: record.canonicalRoot,
      expectedGeneration: record.generation ?? asRootGeneration(1),
      currentGeneration: record.generation
        ? this.currentGeneration(record.canonicalRoot)
        : undefined,
      concretePoolId: this.currentPoolId(),
      currentConcretePoolId: this.currentPoolId(),
    });
  }

  assertFacadeCurrent(
    root: CanonicalRootPath,
    expectedGeneration: RootGeneration | undefined,
    boundary: string,
  ): void {
    this.assertGeneration(root, expectedGeneration, boundary);
  }

  private assertGeneration(
    root: CanonicalRootPath,
    expectedGeneration: RootGeneration | undefined,
    boundary: string,
  ): void {
    if (!this.lifecycleEnabled() || expectedGeneration === undefined) return;
    if (this.isCurrentLiveGeneration(root, expectedGeneration)) return;
    if (this.isRootTombstoned(root, expectedGeneration)) {
      throw new SubcRootReapedError({
        canonicalRoot: root,
        expectedGeneration,
        currentGeneration: this.currentGeneration(root),
        concretePoolId: this.currentPoolId(),
        currentConcretePoolId: this.currentPoolId(),
      });
    }
    this.recordGenerationRejection(root, expectedGeneration, boundary);
    throw this.generationExpiredError(root, expectedGeneration);
  }

  private assertRecordLive(record: SessionRecord): void {
    if (!this.isCurrentSession(record.identityKey, record)) {
      if (record.teardownReason === "root_reaped") throw this.rootReapedError(record);
      throw new RouteTornDownError("subc session closed");
    }
    this.assertGeneration(record.canonicalRoot, record.generation, "route_request");
  }

  private synchronousDemand(root: CanonicalRootPath): boolean {
    const poolId = this.currentPoolId();
    if (!this.lifecycleEnabled() || poolId === undefined) return true;
    if (!this.lifecycleDemandCheck) throw new SubcRootDemandRequiredError(root);
    const result = this.lifecycleDemandCheck(root, poolId);
    if (typeof result === "boolean") return result;
    return result.exists !== false;
  }

  private makeFacade(root: CanonicalRootPath, generation?: RootGeneration): SubcTransport {
    const existing = this.transports.get(root);
    if (existing && (!this.lifecycleEnabled() || existing.getGeneration() === generation)) {
      return existing;
    }
    const transport = new SubcTransport(this, root, generation);
    this.transports.set(root, transport);
    return transport;
  }

  /**
   * Return a live facade. Legacy construction remains synchronous. Lifecycle
   * construction uses the injected synchronous demand seam so a returned facade
   * always has a registered root and captured generation.
   */
  getBridge(projectRoot: string): SubcTransport {
    return this.getBridgeForCanonicalRoot(this.canonicalRoot(projectRoot));
  }

  getBridgeForCanonicalRoot(root: CanonicalRootPath): SubcTransport {
    if (this.shuttingDown && this.lifecycleEnabled()) {
      throw new SubcTransportShuttingDownError();
    }

    if (!this.lifecycleEnabled()) return this.makeFacade(root);

    const current = this.currentGeneration(root);
    const existing = this.transports.get(root);
    if (
      current !== undefined &&
      this.isCurrentLiveGeneration(root, current) &&
      existing?.getGeneration() === current
    ) {
      return existing;
    }

    if (!this.synchronousDemand(root)) throw new SubcRootDemandRequiredError(root);
    const registration = this.lifecycleRegistration;
    const registry = this.lifecycleRegistry;
    if (!registration || !registry) throw new SubcRootDemandRequiredError(root);
    const generation = registry.registerRoot(registration.concretePoolId, root);
    return this.makeFacade(root, generation);
  }

  /** Async demand entry point for registries whose existence seam is asynchronous. */
  async getBridgeForDemand(projectRoot: string): Promise<SubcTransport | null> {
    const root = this.canonicalRoot(projectRoot);
    if (this.shuttingDown || !this.lifecycleEnabled()) return null;
    const registration = this.lifecycleRegistration;
    const registry = this.lifecycleRegistry;
    if (!registration || !registry) return null;
    const generation = await registry.ensureRootForDemand(registration.concretePoolId, root);
    if (
      generation === undefined ||
      !registry.isCurrentLiveGeneration(registration.concretePoolId, root, generation)
    ) {
      return null;
    }
    return this.makeFacade(root, generation);
  }

  /** Alias used by host construction code that calls the seam a demand operation. */
  demandBridge(projectRoot: string): Promise<SubcTransport | null> {
    return this.getBridgeForDemand(projectRoot);
  }

  getActiveBridgeForRoot(projectRoot: string): SubcTransport | null {
    const root = this.canonicalRoot(projectRoot);
    const transport = this.transports.get(root);
    if (!transport) return null;
    if (this.lifecycleEnabled()) {
      return this.isCurrentLiveGeneration(root, transport.getGeneration()) ? transport : null;
    }
    if (!this.client || this.shuttingDown) return null;
    return transport;
  }

  /** Non-creating lookup requiring the complete pool/root/generation provenance. */
  getActiveBridgeForRootGeneration(ref: BgNudgeRef): SubcTransport | null {
    const poolId = this.currentPoolId();
    if (!this.lifecycleEnabled() || poolId === undefined || ref.concretePoolId !== poolId)
      return null;
    if (!this.isCurrentLiveGeneration(ref.canonicalRoot, ref.generation)) return null;
    const transport = this.transports.get(ref.canonicalRoot);
    return transport?.getGeneration() === ref.generation ? transport : null;
  }

  activeBridges(): SubcTransport[] {
    if (this.shuttingDown) return [];
    return [...this.transports.entries()].flatMap(([root, transport]) =>
      !this.lifecycleEnabled() || this.isCurrentLiveGeneration(root, transport.getGeneration())
        ? [transport]
        : [],
    );
  }

  async toolCall(
    projectRoot: string,
    runtime: { sessionID?: string },
    name: string,
    rawArgs: ToolCallArguments = {},
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    return this.getBridge(projectRoot).toolCall(runtime.sessionID, name, rawArgs, options);
  }

  private getOrCreateSession(identity: BindIdentity, generation?: RootGeneration): SessionRecord {
    const key = identityKey(identity);
    const root = asCanonicalRootPath(identity.project_root);
    let record = this.sessions.get(key);
    if (record && !record.closed) {
      if (this.lifecycleEnabled() && record.generation !== generation) {
        throw this.generationExpiredError(root, generation ?? asRootGeneration(1));
      }
      record.lastUsedAt = Date.now();
      return record;
    }
    record = {
      identity,
      identityKey: key,
      canonicalRoot: root,
      generation,
      routeEntry: null,
      retiredRoutes: new Set(),
      bgSub: null,
      closed: false,
      teardownReason: null,
      inflight: 0,
      lastUsedAt: Date.now(),
      observedFromStart: this.observedStarts.delete(key),
      backgroundTasks: new Map(),
      backgroundWatches: new Map(),
      unknownBackgroundWork: false,
      pendingBgNudge: false,
      pendingDetach: false,
    };
    this.sessions.set(key, record);
    let records = this.sessionIndex.get(identity.session);
    if (!records) {
      records = new Set<SessionRecord>();
      this.sessionIndex.set(identity.session, records);
    }
    records.add(record);
    let keys = this.rootIndex.get(root);
    if (!keys) {
      keys = new Set<IdentityKey>();
      this.rootIndex.set(root, keys);
    }
    keys.add(key);
    if (!this.sessionSweep) {
      this.sessionSweep = setInterval(() => {
        void this.reapIdleSessions();
      }, 60_000);
      this.sessionSweep.unref?.();
    }
    return record;
  }

  /** Release only idle carriers, not the daemon's durable session or completion queue.
   * The next request opens a new route and subscribes again; subscription startup
   * always nudges a drain, replaying completions queued while no carrier existed.
   */
  async reapIdleSessions(now = Date.now()): Promise<void> {
    for (const [key, bucket] of this.nudgeDeliveryLogState) {
      for (const [kind, state] of bucket) {
        if (now - state.lastEmittedAt >= 15 * 60_000) bucket.delete(kind);
      }
      if (bucket.size === 0) this.nudgeDeliveryLogState.delete(key);
    }
    const detached: DetachedSession[] = [];
    for (const [key, record] of this.sessions) {
      if (
        // The wire has no session-wide background census. A resumed/rebound
        // session may have tasks or watches this pool never saw, so only a
        // host-observed creation can establish a complete lifetime history.
        !record.observedFromStart ||
        record.backgroundTasks.size > 0 ||
        record.backgroundWatches.size > 0 ||
        record.unknownBackgroundWork ||
        record.pendingBgNudge ||
        record.pendingDetach ||
        record.inflight > 0 ||
        record.retiredRoutes.size > 0 ||
        record.routeEntry?.opening ||
        now - record.lastUsedAt < 15 * 60_000
      )
        continue;
      const owner = this.detachSession(key, "session_closed");
      if (owner) detached.push(owner);
    }
    if (this.sessions.size === 0 && this.sessionSweep) {
      clearInterval(this.sessionSweep);
      this.sessionSweep = null;
    }
    const cleanup = Promise.allSettled(detached.map((owner) => this.cleanupDetached(owner)));
    this.pendingRootCleanups.add(cleanup);
    try {
      await cleanup;
    } finally {
      this.pendingRootCleanups.delete(cleanup);
    }
  }

  observeSessionStart(projectRoot: string, session: string): void {
    if (this.shuttingDown || !session) return;
    const key = identityKey({
      project_root: canonicalizeProjectRoot(projectRoot),
      harness: this.harness,
      session,
    });
    const record = this.sessions.get(key);
    if (record) {
      record.observedFromStart = true;
      return;
    }
    if (this.observedStarts.size >= 256)
      this.observedStarts.delete(this.observedStarts.values().next().value as IdentityKey);
    this.observedStarts.add(key);
  }

  __pendingSessionStartsForTests(): number {
    return this.observedStarts.size;
  }

  private observeBackgroundReply(
    record: SessionRecord,
    body: Record<string, unknown>,
    reply: unknown,
  ): void {
    const name = String(body.name);
    const args = isRecord(body.arguments) ? body.arguments : {};
    const data =
      isRecord(reply) && isRecord(reply.structuredContent) ? reply.structuredContent : null;
    const affectsBackground =
      name === "bash" ||
      name === "powershell" ||
      [
        "bash_status",
        "bash_kill",
        "bash_write",
        "bash_promote",
        "bash_watch",
        "bash_notify",
        "bash_unnotify",
        "bash_wait_detach",
        "bash_drain_completions",
        "bash_ack_completions",
      ].includes(name);
    if (!data || data.success !== true) {
      if (affectsBackground) {
        if (typeof args.task_id === "string") {
          if (!record.backgroundTasks.has(args.task_id))
            record.backgroundTasks.set(args.task_id, "unknown");
        } else record.unknownBackgroundWork = true;
      }
      return;
    }
    if (typeof data.task_id === "string") {
      // Terminal tasks remain pinned until the daemon explicitly ACKs them.
      const terminal = isTerminalStatus(data.status);
      const running = ["starting", "running", "killing"].includes(String(data.status));
      record.backgroundTasks.set(
        data.task_id,
        terminal ? "terminal" : running ? "running" : "unknown",
      );
      if (name === "bash" || name === "powershell") record.pendingDetach = false;
    }
    if (Array.isArray(data.bg_completions)) {
      for (const completion of data.bg_completions) {
        if (isRecord(completion) && typeof completion.task_id === "string")
          record.backgroundTasks.set(completion.task_id, "terminal");
        else record.unknownBackgroundWork = true;
      }
    }
    if (Array.isArray(data.pending_matches)) {
      for (const match of data.pending_matches) {
        if (
          isRecord(match) &&
          typeof match.watch_id === "string" &&
          typeof match.task_id === "string"
        )
          record.backgroundWatches.set(match.watch_id, match.task_id);
        else record.unknownBackgroundWork = true;
      }
    }
    if (
      name === "bash_drain_completions" &&
      Array.isArray(data.bg_completions) &&
      Array.isArray(data.pending_matches)
    )
      record.pendingBgNudge = false;
    if (name === "bash_notify") {
      if (typeof data.watch_id === "string" && typeof args.task_id === "string")
        record.backgroundWatches.set(data.watch_id, args.task_id);
      else record.unknownBackgroundWork = true;
    }
    if (name === "bash_unnotify" && data.unregistered === true && typeof args.watch_id === "string")
      record.backgroundWatches.delete(args.watch_id);
    if (name === "bash_ack_completions" && Array.isArray(data.acked_matches)) {
      for (const match of data.acked_matches) {
        if (!isRecord(match)) continue;
        if (match.watch_removed === true && typeof match.watch_id === "string")
          record.backgroundWatches.delete(match.watch_id);
        // Only the daemon knows whether this match also settled the completion.
        // Removing one watch alone does not prove that the task is settled.
        if (match.task_settled === true && typeof match.task_id === "string")
          record.backgroundTasks.delete(match.task_id);
      }
    }
    if (name === "bash_ack_completions" && Array.isArray(data.acked_task_ids)) {
      // ACK also acknowledges mid-run pattern matches. It does not prove a
      // running/unknown task is terminal, so those must keep their carrier.
      for (const task of data.acked_task_ids)
        if (typeof task === "string" && record.backgroundTasks.get(task) === "terminal") {
          record.backgroundTasks.delete(task);
          // The daemon's terminal ACK also removes every watch on that task.
          for (const [watch, owner] of record.backgroundWatches) {
            if (owner === task) record.backgroundWatches.delete(watch);
          }
        }
    }
    if (name === "bash_wait_detach") {
      if (typeof data.detached === "boolean") record.pendingDetach = data.detached;
      else record.unknownBackgroundWork = true;
    }
  }

  __retainedSessionCountsForTests(): {
    sessions: number;
    roots: number;
    routes: number;
    subscriptions: number;
    sweepTimers: number;
  } {
    return {
      sessions: this.sessions.size,
      roots: this.rootIndex.size,
      routes: [...this.sessions.values()].filter((r) => r.routeEntry !== null).length,
      subscriptions: [...this.sessions.values()].filter((r) => r.bgSub !== null).length,
      sweepTimers: this.sessionSweep ? 1 : 0,
    };
  }

  private isCurrentSession(key: IdentityKey, record: SessionRecord): boolean {
    return this.sessions.get(key) === record && !record.closed;
  }

  private currentSessionForNudge(identity: BindIdentity): SessionRecord | null {
    const current = this.sessions.get(identityKey(identity));
    return current && !current.closed ? current : null;
  }

  private nudgeRefFor(record: SessionRecord): BgNudgeRef | undefined {
    const poolId = this.currentPoolId();
    const generation = record.generation;
    if (poolId === undefined || generation === undefined) return undefined;
    return {
      canonicalRoot: record.canonicalRoot,
      session: record.identity.session,
      concretePoolId: poolId,
      generation,
    };
  }

  private logNudgeDelivery(kind: string, record: SessionRecord, message: string): void {
    let bucket = this.nudgeDeliveryLogState.get(record.identityKey);
    if (!bucket) {
      bucket = new Map();
      this.nudgeDeliveryLogState.set(record.identityKey, bucket);
    }
    const now = Date.now();
    const state = bucket.get(kind);
    if (state && now - state.lastEmittedAt < BG_LIFECYCLE_LOG_INTERVAL_MS) {
      state.suppressed += 1;
      return;
    }
    const suppressed = state?.suppressed ?? 0;
    bucket.set(kind, { lastEmittedAt: now, suppressed: 0 });
    const suffix = suppressed > 0 ? ` suppressed=${suppressed}` : "";
    log(`subc bg_events: ${message}${suffix}`, { sessionId: record.identity.session });
  }

  private removeIndexMembership(record: SessionRecord): void {
    const records = this.sessionIndex.get(record.identity.session);
    records?.delete(record);
    if (records?.size === 0) this.sessionIndex.delete(record.identity.session);
    const keys = this.rootIndex.get(record.canonicalRoot);
    if (!keys) return;
    keys.delete(record.identityKey);
    if (keys.size === 0) this.rootIndex.delete(record.canonicalRoot);
  }

  private deleteSessionIfEmpty(_key: IdentityKey, record: SessionRecord): void {
    if (
      this.sessions.get(record.identityKey) === record &&
      !record.closed &&
      record.inflight === 0 &&
      record.retiredRoutes.size === 0 &&
      record.backgroundTasks.size === 0 &&
      record.backgroundWatches.size === 0 &&
      !record.unknownBackgroundWork &&
      !record.pendingBgNudge &&
      !record.pendingDetach &&
      record.routeEntry === null &&
      record.bgSub === null
    ) {
      this.sessions.delete(record.identityKey);
      this.removeIndexMembership(record);
    }
    this.stopUnusedHostScopeRetry(record.identity.session);
  }

  /**
   * Atomically detaches one opaque identity. Nothing in this method awaits: it
   * is the sole owner transfer used by session close, root reap, and shutdown.
   */
  private detachSession(
    key: IdentityKey,
    reason: "root_reaped" | "session_closed" | "shutdown",
  ): DetachedSession | null {
    const record = this.sessions.get(key);
    if (!record || record.closed) return null;
    record.closed = true;
    record.teardownReason = reason;
    this.sessions.delete(key);
    this.removeIndexMembership(record);
    this.nudgeDeliveryLogState.delete(key);

    const bgSub = record.bgSub;
    record.bgSub = null;
    const routeEntry = record.routeEntry;
    record.routeEntry = null;
    if (routeEntry) routeEntry.closed = true;
    this.stopUnusedHostScopeRetry(record.identity.session);
    return { record, bgSub, routeEntry };
  }

  private async cleanupDetached(detached: DetachedSession): Promise<void> {
    const cleanup: Promise<unknown>[] = [];
    if (detached.bgSub) cleanup.push(detached.bgSub.stop());
    const entries = new Set(detached.record.retiredRoutes);
    detached.record.retiredRoutes.clear();
    if (detached.routeEntry) entries.add(detached.routeEntry);
    for (const entry of entries) {
      entry.closed = true;
      if (entry.handle != null) {
        try {
          cleanup.push(
            Promise.resolve(entry.client.closeRouteChannel(entry.handle)).catch(() => undefined),
          );
        } catch {
          // A client may throw synchronously when its socket is already closed.
        }
      }
    }
    await Promise.allSettled(cleanup);
  }

  private isReapInduced(record: SessionRecord): boolean {
    return record.teardownReason === "root_reaped";
  }

  private annotateReapError(error: unknown, record: SessionRecord): unknown {
    if (!this.isReapInduced(record)) return error;
    if (error instanceof SubcRootReapedError) return error;
    if (error instanceof Error) {
      try {
        Object.defineProperty(error, "subcTeardownReason", {
          configurable: true,
          enumerable: true,
          value: "root_reaped",
          writable: false,
        });
      } catch {
        // A frozen transport error cannot be annotated; use the stable wrapper.
        return this.rootReapedError(record);
      }
      return error;
    }
    return this.rootReapedError(record);
  }

  private closeRetiredRoute(record: SessionRecord, entry: RouteEntry): void {
    if (entry.inflight > 0 || !record.retiredRoutes.delete(entry)) return;
    if (entry.handle != null) safeCloseRoute(entry.client, entry.handle);
    this.deleteSessionIfEmpty(record.identityKey, record);
  }

  private releaseRouteRequest(record: SessionRecord, entry: RouteEntry): void {
    entry.inflight -= 1;
    this.closeRetiredRoute(record, entry);
  }

  private retireHostToolRoute(record: SessionRecord): void {
    const entry = record.routeEntry;
    if (!entry) return;
    entry.closed = true;
    entry.closedBy ??= "route replaced";
    record.routeEntry = null;
    record.retiredRoutes.add(entry);
    this.closeRetiredRoute(record, entry);
    this.stopUnusedHostScopeRetry(record.identity.session);
  }

  /** Open or reuse a route while guarding every lifecycle boundary. */
  async routeRequest(
    identity: BindIdentity,
    body: Record<string, unknown>,
    timeoutMs?: number,
    onProgress?: RequestOptions["onProgress"],
    expectedGeneration?: RootGeneration,
    abortSignal?: AbortSignal,
  ): Promise<unknown> {
    const callStartedAt = performance.now();
    const callDeadlineMs = timeoutMs ?? SUBC_DEFAULT_REQUEST_TIMEOUT_MS;
    // Charge shared backoff delays even when an injected sleeper advances no real time.
    let scheduledDelayMs = 0;
    const spentMs = (): number => Math.max(performance.now() - callStartedAt, scheduledDelayMs);
    const remainingMs = (): number => callDeadlineMs - spentMs();
    const root = asCanonicalRootPath(identity.project_root);
    let generation = expectedGeneration;
    if (this.lifecycleEnabled()) {
      generation ??= this.currentGeneration(root);
      if (generation === undefined) throw new SubcRootDemandRequiredError(root);
      this.assertGeneration(root, generation, "route_request");
    }
    this.assertRootCanAttach(root);
    const key = identityKey(identity);
    const record = this.getOrCreateSession(identity, generation);
    record.inflight += 1;

    try {
      let client: SubcClientLike;
      try {
        let keyChanged: ReconnectKeyChangedError | undefined;
        while (true) {
          const remaining = remainingMs();
          if (remaining <= 0)
            throw keyChanged ?? callDeadlinePassedError(undefined, callDeadlineMs);
          let timer: ReturnType<typeof setTimeout> | undefined;
          try {
            // Each attempt calls SubcClient.connect anew, which re-reads the connection file.
            const acquired = await Promise.race([
              this.ensureClient(),
              new Promise<typeof CALL_DEADLINE_EXPIRED>((resolve) => {
                timer = setTimeout(() => resolve(CALL_DEADLINE_EXPIRED), remaining);
              }),
            ]);
            if (acquired === CALL_DEADLINE_EXPIRED) {
              throw keyChanged ?? callDeadlinePassedError(undefined, callDeadlineMs);
            }
            client = acquired;
            break;
          } catch (error) {
            if (!(error instanceof ReconnectKeyChangedError)) throw error;
            keyChanged = error;
            if (spentMs() + this.nextRouteReopenDelayMs() >= callDeadlineMs) throw error;
          } finally {
            clearTimeout(timer);
          }
          const { delayMs, wait } = this.waitForRouteReopenBackoff();
          scheduledDelayMs = spentMs() + delayMs;
          // A shared retry sleeper may outlive this caller's remaining budget.
          let retryTimer: ReturnType<typeof setTimeout> | undefined;
          try {
            const waited = await Promise.race([
              wait,
              new Promise<typeof CALL_DEADLINE_EXPIRED>((resolve) => {
                retryTimer = setTimeout(
                  () => resolve(CALL_DEADLINE_EXPIRED),
                  callDeadlineMs - (performance.now() - callStartedAt),
                );
              }),
            ]);
            if (waited === CALL_DEADLINE_EXPIRED) throw keyChanged;
          } finally {
            clearTimeout(retryTimer);
          }
        }
        this.assertRecordLive(record);
      } catch (error) {
        throw this.annotateReapError(error, record);
      }

      const openRoute = async (): Promise<{ route: RouteHandle; entry: RouteEntry }> => {
        try {
          this.assertRecordLive(record);
          const opened = await this.routeHandle(client, identity, record);
          this.assertRecordLive(record);
          return opened;
        } catch (error) {
          if (this.isReapInduced(record)) throw this.annotateReapError(error, record);
          if (error instanceof RouteTornDownError) throw error;
          const ownsClient = this.isCurrentSession(key, record) && this.client === client;
          if (ownsClient && isSocketDeathError(error)) {
            this.dropClient(client, {
              error,
              trigger: "route.open",
              session: identity.session,
            });
          } else if (isDaemonAnswerError(error)) {
            // A refusal (cap reached, target unavailable, ...) fails this route
            // only, and proves the shared socket still delivers replies.
            this.noteClientAlive(client);
          } else if (ownsClient && isUnansweredCallError(error)) {
            this.noteUnansweredCall(client, {
              error,
              trigger: "route.open",
              session: identity.session,
            });
          }
          throw error;
        }
      };

      // A module reload (drain, restart, warm-up) refuses route.open for as long
      // as it lasts. The call waits it out for as long as its own deadline allows,
      // but never longer than ROUTE_OPEN_RELOAD_WAIT_CEILING_MS, and then surfaces
      // the refusal (which lets a caller such as bash fall back).
      //
      // Everything the call does counts against its deadline: route opens
      // (a bind relay can hold one for about 12 s), retry sleeps, and the
      // request itself, which is sent with only the time that is left. So the
      // call never outlives the deadline it was given, whichever path it takes.
      const reloadWaitBudgetMs = Math.min(callDeadlineMs, ROUTE_OPEN_RELOAD_WAIT_CEILING_MS);
      // The most recent route refusal, so a deadline that passes mid-open still
      // names the daemon's reason instead of a bare timeout.
      let lastRefusal: unknown;

      // subc-client's routeOpen takes no deadline of its own (only its fixed
      // channel-0 default), so the call stops waiting for the open once its
      // own time is up. An open that completes later still lands in the
      // session's route entry, where the next call reuses it or teardown
      // closes it.
      const openRouteWithinDeadline = async (): Promise<{
        route: RouteHandle;
        entry: RouteEntry;
      }> => {
        const remaining = remainingMs();
        if (remaining <= 0) throw callDeadlinePassedError(lastRefusal, callDeadlineMs);
        let timer: ReturnType<typeof setTimeout> | undefined;
        const expired = new Promise<typeof CALL_DEADLINE_EXPIRED>((resolve) => {
          timer = setTimeout(() => resolve(CALL_DEADLINE_EXPIRED), remaining);
        });
        try {
          const opened = await Promise.race([openRoute(), expired]);
          if (opened === CALL_DEADLINE_EXPIRED) {
            throw callDeadlinePassedError(lastRefusal, callDeadlineMs);
          }
          return opened;
        } finally {
          clearTimeout(timer);
        }
      };

      const openRouteAfterReloadWindow = async (): Promise<{
        route: RouteHandle;
        entry: RouteEntry;
      }> => {
        while (true) {
          try {
            return await openRouteWithinDeadline();
          } catch (error) {
            if (!isRouteOpenReloadWindowError(error)) throw error;
            lastRefusal = error;
            // Check the budget before starting (or joining) the shared retry
            // timer, so a call that gives up leaves no orphan timer behind.
            if (spentMs() + this.nextRouteReopenDelayMs() >= reloadWaitBudgetMs) {
              throw reloadWindowExhaustedError(error, callDeadlineMs);
            }
            const { delayMs, wait } = this.waitForRouteReopenBackoff();
            scheduledDelayMs = spentMs() + delayMs;
            await wait;
          }
        }
      };

      const clearRouteEntry = (entry: RouteEntry): void => {
        if (record.routeEntry !== entry) return;
        this.retireHostToolRoute(record);
      };

      const handleRequestFailure = (error: unknown, entry: RouteEntry): void => {
        const aborted = error instanceof Error && error.name === "AbortError";
        const ownsClient =
          this.isCurrentSession(key, record) &&
          this.client === client &&
          !this.isReapInduced(record);
        if (ownsClient && error instanceof SubcError && error.closeReason?.startsWith("scope_")) {
          this.invalidateHostScope(identity.session);
        }
        if (!aborted && ownsClient && isSocketDeathError(error)) {
          // Dropped before the route is cleared so the drop log counts it.
          this.dropClient(client, {
            error,
            trigger: "request",
            session: identity.session,
            route: routeLabel(entry.handle),
          });
        }
        // A call's timeout or refusal says nothing about the route's health.
        // Retire only a cancelled carrier or one known to be gone; a slow live
        // route remains reusable, and no replacement may cut its other calls.
        if (
          aborted ||
          isRouteProvenAbsentError(error) ||
          (error instanceof SubcError && error.closeReason !== undefined) ||
          isSubcFrameReadTimeoutError(error)
        ) {
          clearRouteEntry(entry);
        }
        if (aborted || !ownsClient || this.client !== client) return;
        if (isRouteProvenAbsentError(error)) return;
        if (isDaemonAnswerError(error)) {
          this.transportFailures = 0;
          this.noteClientAlive(client);
        } else if (isUnansweredCallError(error)) {
          this.noteUnansweredCall(client, {
            error,
            trigger: "request",
            session: identity.session,
            route: routeLabel(entry.handle),
          });
        }
      };

      const requestOnRoute = async (
        route: RouteHandle,
        entry: RouteEntry,
        requestTimeoutMs: number,
      ): Promise<unknown> => {
        this.assertRecordLive(record);
        if (abortSignal?.aborted) {
          clearRouteEntry(entry);
          throw abortedRequestError();
        }

        let rejectAbort: ((error: Error) => void) | undefined;
        const aborted = new Promise<never>((_resolve, reject) => {
          rejectAbort = reject;
        });
        const onAbort = (): void => {
          clearRouteEntry(entry);
          rejectAbort?.(abortedRequestError());
        };
        abortSignal?.addEventListener("abort", onAbort, { once: true });
        try {
          // Track the actual client promise, not the abort race. A cancelled
          // caller has no terminal from the daemon yet, so closing its carrier
          // here would still cut sibling calls and invalidate request credits.
          entry.inflight += 1;
          let request: Promise<unknown>;
          try {
            request = client.request(route, body, {
              timeoutMs: Math.ceil(requestTimeoutMs),
              onProgress,
            });
          } catch (error) {
            this.releaseRouteRequest(record, entry);
            throw error;
          }
          request = request.finally(() => this.releaseRouteRequest(record, entry));
          const reply = abortSignal ? await Promise.race([request, aborted]) : await request;
          // A legacy closeSession may intentionally let an already-delivered reply
          // settle. It must not mutate shared state or recreate a subscription.
          if (!this.isCurrentSession(key, record)) {
            if (this.isReapInduced(record)) throw this.rootReapedError(record);
            return reply;
          }
          this.assertGeneration(record.canonicalRoot, record.generation, "request_completion");
          if (this.client === client) this.transportFailures = 0;
          this.noteClientAlive(client);
          this.observeBackgroundReply(record, body, reply);
          this.ensureBgSubscription(identity, record);
          return reply;
        } finally {
          abortSignal?.removeEventListener("abort", onAbort);
        }
      };

      let routeAndEntry = await openRouteAfterReloadWindow();
      let reopened = false;
      let retriedAbsentRoute = false;
      while (true) {
        // Another call can retire a just-acquired carrier before this caller
        // dispatches. Only already-pending requests belong on retired routes.
        if (routeAndEntry.entry.closed) routeAndEntry = await openRouteAfterReloadWindow();
        // Checked outside the try below so the retry branches never mistake
        // this call's own expiry for a daemon refusal worth retrying.
        const requestTimeoutMs = remainingMs();
        if (requestTimeoutMs <= 0) throw callDeadlinePassedError(lastRefusal, callDeadlineMs);
        try {
          const reply = await requestOnRoute(
            routeAndEntry.route,
            routeAndEntry.entry,
            requestTimeoutMs,
          );
          if (reopened) this.resetRouteReopenBackoff();
          return reply;
        } catch (requestError) {
          const error = frameReadTimeoutError(
            routeClosedMidCallError(
              connectionClosedAfterDispatch(requestError),
              routeAndEntry.entry,
              record,
            ),
          );
          if (this.isReapInduced(record)) throw this.annotateReapError(error, record);
          const ownsRoute = this.isCurrentSession(key, record) && this.client === client;
          if (
            ownsRoute &&
            ((error instanceof SubcError && error.code === "stale_route_epoch") ||
              error instanceof StaleRouteHandleError)
          ) {
            this.invalidateHostScope(identity.session);
          }
          // A route that was bound before a module reload started gets its
          // requests refused by the daemon for as long as the drain lasts. The
          // refusal happens before the request is forwarded, so it is handled
          // like a refused route.open: drop the stale route, then reopen and
          // resend within the same reload-wait budget. Only once that budget is
          // spent does the refusal surface (and let bash fall back to the host).
          // A healthy connection answered, so it does not count as a transport
          // failure.
          if (ownsRoute && isRouteRequestReloadRefusal(error)) {
            clearRouteEntry(routeAndEntry.entry);
            lastRefusal = error;
            if (spentMs() + this.nextRouteReopenDelayMs() >= reloadWaitBudgetMs) {
              throw reloadWindowExhaustedError(error, callDeadlineMs);
            }
            const { delayMs, wait } = this.waitForRouteReopenBackoff();
            scheduledDelayMs = spentMs() + delayMs;
            await wait;
            routeAndEntry = await openRouteAfterReloadWindow();
            reopened = true;
            continue;
          }
          if (ownsRoute && !retriedAbsentRoute && isRouteProvenAbsentError(error)) {
            retriedAbsentRoute = true;
            clearRouteEntry(routeAndEntry.entry);
            // The one resend after a vanished route is still bounded by the
            // call's deadline: its delay is charged like any other, and a
            // delay that would run past the deadline is not taken.
            if (spentMs() + this.nextRouteReopenDelayMs() >= callDeadlineMs) {
              throw callDeadlinePassedError(error, callDeadlineMs);
            }
            const { delayMs, wait } = this.waitForRouteReopenBackoff();
            scheduledDelayMs = spentMs() + delayMs;
            await wait;
            routeAndEntry = await openRouteAfterReloadWindow();
            reopened = true;
            continue;
          }
          handleRequestFailure(error, routeAndEntry.entry);
          noteDispatchedRequestDeadline(error);
          throw error;
        }
      }
    } catch (error) {
      if (this.isReapInduced(record)) throw this.annotateReapError(error, record);
      this.observeBackgroundReply(record, body, null);
      if (record.backgroundTasks.size > 0 || record.unknownBackgroundWork)
        this.ensureBgSubscription(identity, record);
      throw error;
    } finally {
      record.inflight -= 1;
      record.lastUsedAt = Date.now();
      this.deleteSessionIfEmpty(key, record);
    }
  }

  /**
   * Hold a restart burst behind one growing timer before reopening an unknown
   * route. The resend remains bounded to one attempt, while a successful resend
   * resets the next outage to the minimum delay.
   */
  private waitForRouteReopenBackoff(): { delayMs: number; wait: Promise<void> } {
    const pending = this.routeReopenRetry;
    const pendingDelay = this.routeReopenRetryMs;
    if (pending && pendingDelay !== null) return { delayMs: pendingDelay, wait: pending };

    const delayMs = this.routeReopenRetryDelayMs;
    this.routeReopenRetryDelayMs = Math.min(delayMs * 2, RECONNECT_RETRY_CAP_MS);
    let retry!: Promise<void>;
    retry = Promise.resolve()
      .then(() => this.routeRetrySleep(delayMs))
      .finally(() => {
        if (this.routeReopenRetry === retry) {
          this.routeReopenRetry = null;
          this.routeReopenRetryMs = null;
        }
      });
    this.routeReopenRetry = retry;
    this.routeReopenRetryMs = delayMs;
    return { delayMs, wait: retry };
  }

  /** The delay the next `waitForRouteReopenBackoff` call would wait, without starting it. */
  private nextRouteReopenDelayMs(): number {
    if (this.routeReopenRetry && this.routeReopenRetryMs !== null) return this.routeReopenRetryMs;
    return this.routeReopenRetryDelayMs;
  }

  private resetRouteReopenBackoff(): void {
    this.routeReopenRetryDelayMs = RECONNECT_RETRY_FLOOR_MS;
  }

  private async ensureClient(): Promise<SubcClientLike> {
    if (this.shuttingDown) throw new SubcTransportShuttingDownError();
    if (this.client) return this.client;
    if (this.connecting) return this.connecting;
    let owner: SubcClientLike | undefined;
    this.connecting = this.connectFn({
      connectionFile: this.connectionFile,
      handshakeTimeoutMs: this.handshakeTimeoutMs,
      onControlPush: (push) => {
        if (owner && this.client === owner) this.onScopeRouteClose(push);
      },
    })
      .then((client) => {
        this.connecting = null;
        if (this.shuttingDown) {
          try {
            client.close();
          } catch {
            // A late connection is owned by the failed connect and is closed here.
          }
          throw new SubcTransportShuttingDownError();
        }
        this.client = client;
        owner = client;
        this.hadWorkingClient = true;
        this.transportFailures = 0;
        return client;
      })
      .catch((error) => {
        this.connecting = null;
        // A previously authenticated transport can race the daemon's key rotation.
        // Without a prior authenticated client, a proof mismatch can mean an impostor
        // or misconfiguration, so preserve the SDK's permanent authentication error.
        if (this.hadWorkingClient && error instanceof AuthError) {
          throw new ReconnectKeyChangedError();
        }
        throw error;
      });
    return this.connecting;
  }

  private async routeHandle(
    client: SubcClientLike,
    identity: BindIdentity,
    record: SessionRecord,
  ): Promise<{ route: RouteHandle; entry: RouteEntry }> {
    const existing = record.routeEntry;
    if (existing?.handle != null && existing.client === client)
      return { route: existing.handle, entry: existing };
    if (existing?.opening && existing.client === client)
      return { route: await existing.opening, entry: existing };

    const entry: RouteEntry = {
      canonicalRoot: record.canonicalRoot,
      generation: record.generation,
      client,
      opening: null,
      handle: null,
      closed: false,
      inflight: 0,
      closedBy: null,
    };
    this.assertRootCanAttach(record.canonicalRoot);
    const opening = this.openHostToolRoute(client, identity, record, entry)
      .then((route) => {
        if (
          !this.isCurrentSession(record.identityKey, record) ||
          record.routeEntry !== entry ||
          entry.closed ||
          this.client !== client ||
          (this.lifecycleEnabled() &&
            !this.isCurrentLiveGeneration(record.canonicalRoot, record.generation))
        ) {
          safeCloseRoute(client, route);
          if (record.routeEntry === entry) record.routeEntry = null;
          throw new RouteTornDownError("subc route opened after teardown");
        }
        entry.handle = route;
        entry.opening = null;
        this.noteClientAlive(client);
        const state = this.hostScopeCache.get(identity.session);
        if (state) this.scheduleHostScopeRetry(identity.session, state);
        if (
          state?.answer?.result === "scope" &&
          !state.scopeBindPending &&
          !entry.hostScope &&
          !this.hostScopeRefusals.has(identity.session)
        ) {
          this.promoteHostScope(identity.session);
        }
        return route;
      })
      .catch((error) => {
        const currentEntry =
          this.isCurrentSession(record.identityKey, record) && record.routeEntry === entry;
        if (record.routeEntry === entry) {
          entry.closed = true;
          record.routeEntry = null;
        }
        this.stopUnusedHostScopeRetry(identity.session);
        if (
          !this.isCurrentSession(record.identityKey, record) &&
          !(error instanceof RouteTornDownError)
        ) {
          throw new RouteTornDownError("subc route opened after session closed");
        }
        if (currentEntry && isAbsentRootRouteError(error)) {
          this.markRootDormant(record.canonicalRoot, error);
        }
        throw error;
      });
    entry.opening = opening;
    record.routeEntry = entry;
    return { route: await opening, entry };
  }

  private hostScope(client: SubcClientLike, identity: BindIdentity): Promise<HostScopeDecision> {
    let lookupReason = "cache_miss";
    const refused = this.hostScopeRefusals.get(identity.session);
    if (refused) {
      // A refusal applies to one route-binding attempt, not to the session's future scope.
      // Look up the host scope again on the next bind instead of leaving later routes unscoped.
      const previous = this.hostScopeCache.get(identity.session);
      if (previous) this.cancelHostScopeRetry(previous);
      this.hostScopeCache.delete(identity.session);
      this.hostScopeRefusals.delete(identity.session);
      lookupReason = `previous_bind_refusal code=${refused}`;
    }
    let state = this.hostScopeCache.get(identity.session);
    if (!state) {
      state = { retryAt: 0, retryMs: HOST_SCOPE_RETRY_FLOOR_MS };
      this.hostScopeCache.set(identity.session, state);
    }
    if (state.answer) {
      const reason = state.scopeBindPending
        ? "scope_sync_pending"
        : `cached_${state.answer.result}`;
      log(`subc head scope: session=${identity.session} lookup=skipped reason=${reason}`);
      return Promise.resolve({
        scope:
          state.answer.result === "scope" && !state.scopeBindPending
            ? state.answer.scope
            : undefined,
        reason,
      });
    }
    if (state.pending) {
      log(`subc head scope: session=${identity.session} lookup=skipped reason=inflight_lookup`);
      return state.pending;
    }
    // Backoff skips the lookup, not the bind, so an unavailable core cannot delay every root.
    if (Date.now() < state.retryAt) {
      log(
        `subc head scope: session=${identity.session} lookup=skipped reason=backoff retry_in=${state.retryAt - Date.now()}ms`,
      );
      return Promise.resolve({ reason: state.unscopedReason ?? "lookup_backoff" });
    }
    log(`subc head scope: session=${identity.session} lookup=started reason=${lookupReason}`);
    const current = state;
    clearTimeout(current.retryTimer);
    current.retryTimer = undefined;
    const abort = new AbortController();
    current.lookupAbort = abort;
    const lookup = this.readHostScope(client, identity, abort.signal).then((outcome) => {
      // A scope-close push may invalidate the session while the management request is in flight.
      if (this.hostScopeCache.get(identity.session) !== current)
        return { reason: "lookup_invalidated" };
      current.pending = undefined;
      current.lookupAbort = undefined;
      if (abort.signal.aborted) return { reason: "lookup_cancelled" };
      this.logHostScopeOutcome(identity.session, outcome);
      const reason =
        outcome.result === "error"
          ? `lookup_error code=${outcome.code}`
          : `lookup_${outcome.result}`;
      if (outcome.result === "scope" || outcome.result === "no_scope") {
        current.answer = outcome;
        current.retryAt = 0;
        current.retryMs = HOST_SCOPE_RETRY_FLOOR_MS;
        if (outcome.result === "scope") {
          // After a refusal, confirm daemon acceptance before replacing working fallback routes.
          current.promoteAfterBind = Boolean(refused);
          if (!current.promoteAfterBind) this.promoteHostScope(identity.session);
        }
      } else {
        current.unscopedReason = reason;
        current.retryAt = Date.now() + current.retryMs;
        current.retryMs = Math.min(current.retryMs * 2, HOST_SCOPE_RETRY_CAP_MS);
        this.scheduleHostScopeRetry(identity.session, current);
      }
      return {
        scope: outcome.result === "scope" ? outcome.scope : undefined,
        reason,
      };
    });
    current.pending = lookup;
    return lookup;
  }

  private unscopedHostRecord(session: string): SessionRecord | undefined {
    for (const record of this.sessionIndex.get(session) ?? []) {
      const entry = record.routeEntry;
      if (
        record.identity.session === session &&
        !record.closed &&
        ((entry &&
          !entry.closed &&
          !entry.hostScope &&
          entry.client === this.client &&
          (entry.handle || entry.opening)) ||
          (record.bgSub?.attachedClient === this.client && record.bgSub?.attachedRoute))
      )
        return record;
    }
    return undefined;
  }

  private cancelHostScopeRetry(state: HostScopeState): void {
    clearTimeout(state.retryTimer);
    state.retryTimer = undefined;
    state.lookupAbort?.abort();
  }

  private stopUnusedHostScopeRetry(session: string): void {
    const state = this.hostScopeCache.get(session);
    if (state && !this.unscopedHostRecord(session)) {
      if (state.retryTimer || (state.scopeBindPending && state.bindPending))
        log(`subc head scope: session=${session} retry stopped reason=route_closed`);
      this.cancelHostScopeRetry(state);
      state.scopeBindPending = false;
    }
  }

  private scheduleHostScopeRetry(session: string, state: HostScopeState): void {
    if (
      this.shuttingDown ||
      this.hostScopeCache.get(session) !== state ||
      (state.answer && !state.scopeBindPending) ||
      state.pending ||
      state.bindPending ||
      state.retryTimer ||
      this.hostScopeRefusals.has(session) ||
      !this.unscopedHostRecord(session)
    )
      return;
    // A head can keep its tool route for its whole life, so recovery cannot rely on another bind.
    state.retryTimer = setTimeout(
      () => {
        state.retryTimer = undefined;
        const record = this.unscopedHostRecord(session);
        const client = record?.routeEntry?.client ?? record?.bgSub?.attachedClient;
        if (this.shuttingDown || this.hostScopeCache.get(session) !== state || !record || !client)
          return;
        let retry: Promise<unknown>;
        if (state.scopeBindPending && state.answer?.result === "scope") {
          state.bindPending = this.retryHostScopeBind(
            client,
            record.identity,
            state,
            state.answer.scope,
          ).finally(() => {
            state.bindPending = undefined;
            this.scheduleHostScopeRetry(session, state);
          });
          retry = state.bindPending;
        } else {
          retry = this.hostScope(client, record.identity);
        }
        void retry.catch((error) => {
          warn(
            `subc head scope: session=${session} background retry failed code=${error instanceof Error ? error.name : "Error"}`,
          );
        });
      },
      Math.max(0, state.retryAt - Date.now()),
    );
    state.retryTimer.unref();
  }

  private deferHostScopeBind(session: string, state: HostScopeState, code: string): void {
    state.scopeBindPending = true;
    if (Date.now() >= state.retryAt) {
      state.retryAt = Date.now() + state.retryMs;
      state.retryMs = Math.min(state.retryMs * 2, HOST_SCOPE_RETRY_CAP_MS);
    }
    warn(
      `subc head scope: session=${session} scoped bind refused code=${code}; binding unscoped; retry in=${Math.max(0, state.retryAt - Date.now())}ms`,
    );
    this.scheduleHostScopeRetry(session, state);
  }

  private refuseHostScopeBind(
    session: string,
    state: HostScopeState | undefined,
    code: string,
  ): void {
    if (!this.hostScopeRefusals.has(session)) {
      this.hostScopeRefusals.set(session, code);
      warn(
        `subc head scope: session=${session} scoped bind refused code=${code}; retry stopped until next bind lookup`,
      );
    }
    if (state) {
      state.scopeBindPending = false;
      this.cancelHostScopeRetry(state);
    }
    // Remember the downgrade without misreporting it as core's no_scope answer.
  }

  private async retryHostScopeBind(
    client: SubcClientLike,
    identity: BindIdentity,
    state: HostScopeState,
    scope: RouteScope,
  ): Promise<void> {
    try {
      // Validate daemon acceptance before retiring a working fallback route or its held requests.
      const route = await this.bindHostToolRoute(
        client,
        identity,
        "scope_probe",
        "scope_sync_retry",
        scope,
      );
      safeCloseRoute(client, route);
      if (
        this.hostScopeCache.get(identity.session) !== state ||
        !state.scopeBindPending ||
        this.shuttingDown ||
        client !== this.client ||
        !this.unscopedHostRecord(identity.session)
      )
        return;
      state.scopeBindPending = false;
      state.promoteAfterBind = false;
      state.retryAt = 0;
      state.retryMs = HOST_SCOPE_RETRY_FLOOR_MS;
      log(
        `subc head scope: session=${identity.session} scoped bind recovered ref=${scope.ref.slice(0, 8)} epoch=${scope.scopeEpoch}`,
      );
      this.promoteHostScope(identity.session);
    } catch (error) {
      if (
        this.hostScopeCache.get(identity.session) !== state ||
        !state.scopeBindPending ||
        this.shuttingDown ||
        !this.unscopedHostRecord(identity.session)
      )
        return;
      if (
        error instanceof SubcError &&
        error.code &&
        !isSocketDeathError(error) &&
        !isRouteOpenReloadWindowError(error) &&
        !isHostScopeSyncRefusal(error.code)
      ) {
        this.refuseHostScopeBind(identity.session, state, error.code);
      } else {
        this.deferHostScopeBind(
          identity.session,
          state,
          error instanceof SubcError ? (error.code ?? error.name) : "Error",
        );
      }
    }
  }

  private promoteHostScope(session: string): void {
    for (const record of this.sessions.values()) {
      const entry = record.routeEntry;
      if (record.identity.session !== session) continue;
      // Retirement stops new dispatches; the existing request counter defers closure of held waits.
      if (entry?.handle && !entry.hostScope) this.retireHostToolRoute(record);
      const sub = record.bgSub;
      if (!sub?.attachedRoute) continue;
      // The old subscription must finish closing before its normal attach path opens a scoped one.
      void sub.stop().then(() => {
        if (record.bgSub !== sub) return;
        record.bgSub = null;
        this.ensureBgSubscription(record.identity, record);
      });
    }
  }

  private logHostScopeOutcome(session: string, outcome: HostScopeResult): void {
    let key: string;
    let detail: string;
    switch (outcome.result) {
      case "scope":
        key = JSON.stringify([outcome.result, outcome.scope.ref, outcome.scope.scopeEpoch]);
        detail = `scope ref=${outcome.scope.ref.slice(0, 8)} epoch=${outcome.scope.scopeEpoch}`;
        break;
      case "error":
        key = JSON.stringify([outcome.result, outcome.code]);
        detail = `error code=${outcome.code} after=${outcome.afterMs}`;
        break;
      case "timeout":
        key = outcome.result;
        detail = `timeout after=${outcome.afterMs}`;
        break;
      default:
        key = outcome.result;
        detail = outcome.result;
    }
    // Elapsed time varies on every retry; only a changed answer or error code merits another line.
    if (this.hostScopeOutcomes.get(session) === key) return;
    this.hostScopeOutcomes.set(session, key);
    log(`subc head scope: session=${session} result=${detail}`);
  }

  private async readHostScope(
    client: SubcClientLike,
    identity: BindIdentity,
    signal: AbortSignal,
  ): Promise<HostScopeResult> {
    const startedAt = Date.now();
    const timeout = (): HostScopeResult => ({ result: "timeout", afterMs: Date.now() - startedAt });
    let expired = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const abort = new AbortController();
    let cancel!: () => void;
    const cancelled = new Promise<HostScopeResult>((resolve) => {
      cancel = () => {
        expired = true;
        abort.abort();
        resolve({ result: "error", code: "AbortError", afterMs: Date.now() - startedAt });
      };
      signal.addEventListener("abort", cancel, { once: true });
    });
    const read = async (): Promise<HostScopeResult> => {
      let route: RouteHandle | undefined;
      try {
        route = await client.routeOpen(
          { kind: "management_surface", module_id: "prefrontal-core" },
          identity,
          { consumerIdentity: this.consumerIdentity },
        );
        if (expired) return timeout();
        const envelope = await client.request(
          route,
          {
            // Core decodes every management request as {method, params}.
            method: "session.scope_identity",
            params: { harness: identity.harness, session: identity.session },
          },
          { timeoutMs: HOST_SCOPE_LOOKUP_TIMEOUT_MS, signal: abort.signal },
        );
        // Core answers a management request as {result: <answer>}.
        const reply = isRecord(envelope) ? envelope.result : undefined;
        if (isRecord(reply) && reply.answer === "no_scope") return { result: "no_scope" };
        if (
          !isRecord(reply) ||
          !isRecord(reply.owner) ||
          reply.owner.kind !== "reserved" ||
          reply.owner.module_id !== "prefrontal-core" ||
          typeof reply.ref !== "string" ||
          reply.ref.length === 0 ||
          !Number.isSafeInteger(reply.scope_epoch) ||
          (reply.scope_epoch as number) < 0
        )
          return { result: "malformed" };
        return {
          result: "scope",
          scope: {
            owner: { kind: "reserved", module_id: "prefrontal-core" },
            ref: reply.ref,
            scopeEpoch: reply.scope_epoch as number,
          },
        };
      } catch (error) {
        // A missing or unavailable scope service must not disable the host's tools.
        if (expired) return timeout();
        return {
          result: "error",
          code:
            error instanceof SubcError && error.code
              ? error.code
              : error instanceof Error
                ? error.name
                : "Error",
          afterMs: Date.now() - startedAt,
        };
      } finally {
        if (route) safeCloseRoute(client, route);
      }
    };
    try {
      // Keep the two-second bind budget during core startup; fallback routes retry in the background.
      return await Promise.race([
        read(),
        cancelled,
        new Promise<HostScopeResult>((resolve) => {
          timer = setTimeout(() => {
            expired = true;
            abort.abort();
            resolve(timeout());
          }, HOST_SCOPE_LOOKUP_TIMEOUT_MS);
        }),
      ]);
    } finally {
      clearTimeout(timer);
      signal.removeEventListener("abort", cancel);
    }
  }

  private openHostToolRoute(
    client: SubcClientLike,
    identity: BindIdentity,
    record: SessionRecord,
    entry?: Pick<RouteEntry, "hostScope">,
    channel: HostRouteChannel = "tool",
  ): Promise<RouteHandle> {
    if (
      !this.hostScopes ||
      !["opencode", "pi"].includes(identity.harness) ||
      identity.session === DEFAULT_SESSION_ID
    ) {
      const reason = !this.hostScopes
        ? "host_scopes_disabled"
        : identity.session === DEFAULT_SESSION_ID
          ? "default_session"
          : "unsupported_harness";
      log(`subc head scope: session=${identity.session} lookup=skipped reason=${reason}`);
      return this.bindHostToolRoute(client, identity, channel, reason);
    }
    return this.openScopedHostToolRoute(client, identity, record, entry, channel);
  }

  private bindHostToolRoute(
    client: SubcClientLike,
    identity: BindIdentity,
    channel: HostRouteChannel,
    reason: string,
    scope?: RouteScope,
  ): Promise<RouteHandle> {
    log(
      `subc head scope: session=${identity.session} bind=${scope ? "scoped" : "unscoped"} channel=${channel} root=${identity.project_root} reason=${reason}` +
        (scope ? ` ref=${scope.ref.slice(0, 8)} epoch=${scope.scopeEpoch}` : ""),
    );
    return client.routeOpen({ kind: "tool_provider", module_id: AFT_MODULE_ID }, identity, {
      consumerIdentity: this.consumerIdentity,
      ...(scope ? { scope } : {}),
    });
  }

  private async openScopedHostToolRoute(
    client: SubcClientLike,
    identity: BindIdentity,
    record: SessionRecord,
    entry?: Pick<RouteEntry, "hostScope">,
    channel: HostRouteChannel = "tool",
  ): Promise<RouteHandle> {
    const decision = await this.hostScope(client, identity);
    const scope = decision.scope;
    let reason = decision.reason;
    this.assertRecordLive(record);
    if (scope) {
      try {
        const route = await this.bindHostToolRoute(client, identity, channel, reason, scope);
        if (entry) entry.hostScope = scope;
        const state = this.hostScopeCache.get(identity.session);
        if (
          state?.promoteAfterBind &&
          state.answer?.result === "scope" &&
          state.answer.scope === scope
        ) {
          state.promoteAfterBind = false;
          this.promoteHostScope(identity.session);
        }
        return route;
      } catch (error) {
        log(
          `subc head scope: session=${identity.session} bind=failed channel=${channel} reason=scoped_open_error code=${error instanceof SubcError ? (error.code ?? error.name) : "Error"}`,
        );
        if (!(error instanceof SubcError) || !error.code || isSocketDeathError(error)) throw error;
        // A restarting provider has not rejected the scope. Keep it for the normal reopen retry.
        if (isRouteOpenReloadWindowError(error)) throw error;
        this.assertRecordLive(record);
        const state = this.hostScopeCache.get(identity.session);
        if (state && isHostScopeSyncRefusal(error.code)) {
          this.deferHostScopeBind(identity.session, state, error.code);
          reason = `scope_sync_pending code=${error.code}`;
        } else {
          this.refuseHostScopeBind(identity.session, state, error.code);
          // Look up the host scope again after a cached scoped route is refused,
          // before opening an unscoped route.
          if (decision.reason === "cached_scope") {
            return this.openScopedHostToolRoute(client, identity, record, entry, channel);
          }
          reason = `scoped_bind_refused code=${error.code}`;
        }
      }
    }
    this.assertRecordLive(record);
    const route = await this.bindHostToolRoute(client, identity, channel, reason);
    // Scope recovery can finish while a fallback bind waits for the provider, including bg_events.
    if (
      this.hostScopeCache.get(identity.session)?.answer?.result === "scope" &&
      !this.hostScopeCache.get(identity.session)?.scopeBindPending &&
      !this.hostScopeRefusals.has(identity.session)
    ) {
      safeCloseRoute(client, route);
      return this.openScopedHostToolRoute(client, identity, record, entry, channel);
    }
    return route;
  }

  private invalidateHostScope(session: string): void {
    const state = this.hostScopeCache.get(session);
    if (state) {
      if (state.scopeBindPending)
        log(`subc head scope: session=${session} retry stopped reason=scope_route_closed`);
      this.cancelHostScopeRetry(state);
    }
    this.hostScopeCache.delete(session);
    this.hostScopeRefusals.delete(session);
    for (const record of this.sessions.values()) {
      if (record.identity.session !== session || !record.routeEntry) continue;
      this.retireHostToolRoute(record);
    }
  }

  private onScopeRouteClose(push: ControlPush): void {
    if (push.op !== "route.closed") return;
    const reason = routeCloseReason(push);
    if (!reason?.startsWith("scope_")) {
      for (const record of this.sessions.values()) {
        const route = record.routeEntry?.handle;
        if (!route) continue;
        if (
          (Array.isArray(push.body.channels) && push.body.channels.includes(route.channel)) ||
          (!Array.isArray(push.body.channels) && push.body.module_id === AFT_MODULE_ID)
        )
          this.retireHostToolRoute(record);
      }
      return;
    }
    const sessions = new Set<string>();
    for (const record of this.sessions.values()) {
      const routes = [record.routeEntry?.handle, record.bgSub?.attachedRoute].filter(
        (route): route is RouteHandle => route != null,
      );
      if (
        (Array.isArray(push.body.channels) &&
          routes.some((route) => (push.body.channels as unknown[]).includes(route.channel))) ||
        (!Array.isArray(push.body.channels) && push.body.module_id === AFT_MODULE_ID)
      )
        sessions.add(record.identity.session);
    }
    for (const session of sessions) this.invalidateHostScope(session);
  }

  private ensureBgSubscription(identity: BindIdentity, record: SessionRecord): void {
    if (this.shuttingDown || (!this.onBgEventsNudge && !this.onBgEventsNudgeRef)) return;
    if (!this.isCurrentSession(record.identityKey, record)) return;
    if (record.bgSub) return;
    if (!this.rootCanAttach(record.canonicalRoot)) return;

    const nudgeRef = this.nudgeRefFor(record);
    const dispatch = (callback: () => unknown): void => {
      const failed = (error: unknown): void =>
        warn(`subc bg nudge delivery failed: ${describeFailure(error)}`);
      try {
        void Promise.resolve(callback()).catch(failed);
      } catch (error) {
        failed(error);
      }
    };
    const onNudge = (observedEvent: boolean): void => {
      const currentRecord = this.currentSessionForNudge(identity);
      if (!currentRecord) {
        this.logNudgeDelivery(
          "drop-no-current-session",
          record,
          `nudge dropped cause=no-current-session root=${record.canonicalRoot}`,
        );
        return;
      }
      if (currentRecord !== record) {
        this.logNudgeDelivery(
          "forward-superseded-carrier",
          currentRecord,
          `nudge forwarding cause=superseded-carrying-record root=${currentRecord.canonicalRoot}`,
        );
      }
      currentRecord.lastUsedAt = Date.now();
      if (observedEvent) currentRecord.pendingBgNudge = true;

      const currentRef = this.nudgeRefFor(currentRecord);
      let delivered = false;
      if (currentRef && this.onBgEventsNudgeRef) {
        dispatch(() => this.onBgEventsNudgeRef?.(currentRef));
        delivered = true;
      }
      if (this.onBgEventsNudge) {
        if (!currentRef && this.onBgEventsNudgeRef) {
          this.logNudgeDelivery(
            "fallback-missing-generation",
            currentRecord,
            `nudge dispatch fallback=root-session-handler cause=generation-provenance-unavailable root=${currentRecord.canonicalRoot}`,
          );
        }
        dispatch(() =>
          this.onBgEventsNudge?.(
            currentRecord.identity.project_root,
            currentRecord.identity.session,
          ),
        );
        delivered = true;
      }
      if (delivered) return;

      this.logNudgeDelivery(
        "drop-no-compatible-handler",
        currentRecord,
        `nudge dropped cause=generation-provenance-unavailable-and-root-session-handler-unwired root=${currentRecord.canonicalRoot}`,
      );
    };
    const binding: Pick<RouteEntry, "hostScope"> = {};
    let sub: BgSubscription | null = null;
    const clearDormantSubscription = (): void => {
      if (sub && record.bgSub === sub) record.bgSub = null;
    };
    sub = new BgSubscription(
      identity,
      () => this.ensureClient(),
      (client, cause) => this.dropClient(client, cause),
      (client) => {
        binding.hostScope = undefined;
        return this.openHostToolRoute(client, identity, record, binding, "bg_events");
      },
      onNudge,
      this.bgBackoffSleep,
      () => this.rootCanAttach(record.canonicalRoot),
      (error) => {
        if (!isAbsentRootRouteError(error)) return false;
        this.markRootDormant(record.canonicalRoot, error);
        return true;
      },
      clearDormantSubscription,
      this.bgDispatchProbeIntervalMs,
      nudgeRef,
      () =>
        this.isCurrentSession(record.identityKey, record) &&
        this.isCurrentLiveGeneration(record.canonicalRoot, record.generation),
      (client) => this.noteClientAlive(client),
      (client, cause) => this.noteUnansweredCall(client, cause),
      () => {
        const state = this.hostScopeCache.get(identity.session);
        if (state) this.scheduleHostScopeRetry(identity.session, state);
        if (
          state?.answer?.result === "scope" &&
          !state.scopeBindPending &&
          !binding.hostScope &&
          !this.hostScopeRefusals.has(identity.session)
        ) {
          this.promoteHostScope(identity.session);
        }
      },
    );
    record.bgSub = sub;
    if (!this.rootCanAttach(record.canonicalRoot)) {
      record.bgSub = null;
      void sub.stop();
    }
  }

  /**
   * Invalidate only routes owned by a dead client. Sessions remain indexed so a
   * replacement client can reconnect the same identity and its bg subscription.
   *
   * Only socket death reaches here (see {@link isSocketDeathError}), or a run of
   * unanswered calls that a liveness probe then confirmed. Every drop takes all
   * sessions' routes down with it, so each one logs its cause and its reach.
   */
  private dropClient(client: SubcClientLike, cause: ClientDropCause): void {
    if (this.client !== client) return;
    let sessions = 0;
    let routes = 0;
    for (const record of this.sessions.values()) {
      const toolRoute = record.routeEntry?.client === client ? 1 : 0;
      const bgRoute = record.bgSub?.attachedClient === client ? 1 : 0;
      if (toolRoute + bgRoute > 0) sessions += 1;
      routes += toolRoute + bgRoute;
    }
    const message = cause.error instanceof Error ? cause.error.message : String(cause.error);
    warn(
      `subc pool: dropping shared client cause=${describeFailure(cause.error)} trigger=${cause.trigger}` +
        (cause.session ? ` session=${cause.session}` : "") +
        (cause.route ? ` route=${cause.route}` : "") +
        ` sessions=${sessions} routes=${routes}` +
        (cause.detail ? ` ${cause.detail}` : "") +
        ` message=${JSON.stringify(message)}`,
    );
    this.client = null;
    for (const record of this.sessions.values()) {
      for (const retired of record.retiredRoutes) {
        if (retired.client !== client) continue;
        retired.closed = true;
        record.retiredRoutes.delete(retired);
      }
      const entry = record.routeEntry;
      if (entry?.client !== client) continue;
      entry.closed = true;
      record.routeEntry = null;
      this.deleteSessionIfEmpty(record.identityKey, record);
    }
    for (const session of this.hostScopeCache.keys()) this.stopUnusedHostScopeRetry(session);
    this.transportFailures = 0;
    try {
      client.close();
    } catch {
      // A dead socket is already released by the peer.
    }
  }

  /** Record that the current client delivered a reply, which proves its socket is alive. */
  private noteClientAlive(client: SubcClientLike): void {
    if (this.client === client) this.clientLivenessMark += 1;
  }

  /**
   * Count a call on `client` that failed without proving the socket dead. After
   * MAX_CONSECUTIVE_TRANSPORT_FAILURES in a row the connection is checked with a
   * liveness probe; the count alone never closes it.
   */
  private noteUnansweredCall(client: SubcClientLike, cause: ClientDropCause): void {
    if (this.client !== client) return;
    if (++this.transportFailures < MAX_CONSECUTIVE_TRANSPORT_FAILURES) return;
    this.transportFailures = 0;
    this.probeClientLiveness(client, cause);
  }

  /**
   * Decide whether a connection that stopped answering calls is dead. A channel-0
   * catalog.list round trip is sent on the same connection; the client is dropped
   * only when that probe fails with socket death, or goes unanswered for the
   * probe window while no other reply arrived on the connection either. Any
   * reply, including a refusal, keeps the client.
   */
  private probeClientLiveness(client: SubcClientLike, suspicion: ClientDropCause): void {
    if (this.livenessProbe || this.client !== client) return;
    const suspicionText = `after ${MAX_CONSECUTIVE_TRANSPORT_FAILURES} consecutive unanswered calls; last ${suspicion.trigger} failed with ${describeFailure(suspicion.error)}`;
    if (typeof client.catalogList !== "function") {
      log(`subc pool: keeping shared client ${suspicionText}; the client offers no liveness probe`);
      return;
    }
    const mark = this.clientLivenessMark;
    const windowMs = this.livenessProbeTimeoutMs;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const timedOut = new Promise<"timeout">((resolve) => {
      timer = setTimeout(() => resolve("timeout"), windowMs);
      (timer as { unref?: () => void }).unref?.();
    });
    const probe = (async (): Promise<void> => {
      let deathEvidence: unknown = null;
      try {
        const reply = Promise.resolve().then(() => client.catalogList?.(AFT_MODULE_ID));
        const outcome = await Promise.race([reply.then(() => "answered" as const), timedOut]);
        if (outcome === "timeout") deathEvidence = new SubcLivenessProbeTimeoutError(windowMs);
      } catch (error) {
        // Only socket death or the probe's own deadline convicts. Any other
        // rejection came back over the connection, which is proof it works.
        if (
          isSocketDeathError(error) ||
          (error instanceof SubcError && error.code === REQUEST_DEADLINE_CODE)
        ) {
          deathEvidence = error;
        }
      } finally {
        clearTimeout(timer);
      }
      if (this.client !== client) return;
      if (deathEvidence !== null && this.clientLivenessMark === mark) {
        this.dropClient(client, {
          error: deathEvidence,
          trigger: "liveness probe",
          session: suspicion.session,
          route: suspicion.route,
          detail: suspicionText,
        });
        return;
      }
      this.transportFailures = 0;
      log(`subc pool: keeping shared client ${suspicionText}; the connection answered`);
    })().finally(() => {
      if (this.livenessProbe === probe) this.livenessProbe = null;
    });
    this.livenessProbe = probe;
  }

  /** Synchronous concrete-facade eviction used by the registry coordinator. */
  evictConcreteFacade(root: CanonicalRootPath, generation: RootGeneration): void {
    const facade = this.transports.get(root);
    if (facade?.getGeneration() === generation) this.transports.delete(root);
  }

  /**
   * Registry-owned coordinated close. All record/index/facade mutations happen
   * before cleanup promises are created; the registry has already tombstoned the
   * matching generation and evicted the wrapper facade before this is called.
   */
  async closeProjectRoot(
    root: CanonicalRootPath,
    generation: RootGeneration,
  ): Promise<{ tornDownSessionCount: number; tornDownFacadeCount: number }> {
    if (!this.lifecycleEnabled()) return { tornDownSessionCount: 0, tornDownFacadeCount: 0 };
    if (!this.isRootTombstoned(root, generation))
      return { tornDownSessionCount: 0, tornDownFacadeCount: 0 };

    const keys = [...(this.rootIndex.get(root) ?? new Set<IdentityKey>())];
    const detached: DetachedSession[] = [];
    for (const key of keys) {
      const record = this.sessions.get(key);
      if (record?.canonicalRoot === root && record.generation === generation) {
        const owner = this.detachSession(key, "root_reaped");
        if (owner) detached.push(owner);
      }
    }
    this.rootIndex.delete(root);
    const hadFacade = this.transports.get(root)?.getGeneration() === generation;
    this.evictConcreteFacade(root, generation);

    const cleanup = Promise.allSettled(detached.map((owner) => this.cleanupDetached(owner))).then(
      () => ({
        tornDownSessionCount: detached.length,
        tornDownFacadeCount: hadFacade ? 1 : 0,
      }),
    );
    this.pendingRootCleanups.add(cleanup);
    void cleanup.finally(() => this.pendingRootCleanups.delete(cleanup));
    return cleanup;
  }

  /** Forward explicit lifecycle close requests to the registry-owned coordinator. */
  requestProjectRootClose(
    root: CanonicalRootPath,
    generation: RootGeneration,
    cause: "sweep" | "explicit" = "explicit",
  ): Promise<void> {
    const registry = this.lifecycleRegistry;
    const registration = this.lifecycleRegistration;
    if (!registry || !registration || !registration.reapingEnabled) return Promise.resolve();
    return registry.requestProjectRootClose(registration.concretePoolId, root, generation, cause);
  }

  /**
   * Subc reads config locally, but plugin registration facts are process state
   * and must accompany route requests because RouteBind has no such field.
   */
  setConfigureOverride(key: string, value: unknown): void {
    if (key !== "edit_slot_survives") return;
    if (typeof value !== "boolean") {
      throw new Error("edit_slot_survives must be set once to a boolean");
    }
    if (this.editSlotSurvivesCaptured) {
      throw new Error("edit_slot_survives is write-once and was already captured");
    }
    this.editSlotSurvives = value;
    this.editSlotSurvivesCaptured = true;
  }

  getEditSlotSurvives(): boolean | undefined {
    return this.editSlotSurvives;
  }

  /** No-op over subc: the daemon owns the live module's configure lifecycle. */
  async reconfigure(_projectRoot: string, _overrides: Record<string, unknown>): Promise<void> {}

  /** No-op over subc: the daemon supervises the binary, not the plugin. */
  async replaceBinary(path: string): Promise<string> {
    return path;
  }

  isShutdown(): boolean {
    return this.shuttingDown;
  }

  async shutdown(): Promise<void> {
    if (this.shuttingDown) return;
    this.shuttingDown = true;
    if (this.sessionSweep) clearInterval(this.sessionSweep);
    this.sessionSweep = null;
    this.nudgeDeliveryLogState.clear();
    this.generationRejections.clear();
    this.observedStarts.clear();
    this.dormantRoots.clear();
    const scopeLookups = [...this.hostScopeCache.values()].flatMap((state) => {
      this.cancelHostScopeRetry(state);
      return state.pending ? [state.pending] : [];
    });
    this.hostScopeCache.clear();
    this.hostScopeRefusals.clear();
    this.hostScopeOutcomes.clear();

    // Deregistration synchronously tombstones all registered roots and invokes
    // closeProjectRoot before this method reaches its first await.
    const registration = this.lifecycleRegistration;
    registration?.deregister();
    this.lifecycleRegistration = null;

    const detached: DetachedSession[] = [];
    for (const key of [...this.sessions.keys()]) {
      const owner = this.detachSession(key, "shutdown");
      if (owner) detached.push(owner);
    }
    this.rootIndex.clear();
    this.sessionIndex.clear();
    this.transports.clear();

    const client = this.client;
    this.client = null;
    const pending = [...this.pendingRootCleanups];
    await Promise.allSettled([
      ...detached.map((owner) => this.cleanupDetached(owner)),
      ...pending,
      ...scopeLookups,
    ]);
    if (client) {
      try {
        client.close();
      } catch {
        // Best effort; route cleanup has already been attempted.
      }
    }
  }

  async closeSession(projectRoot: string, session: string): Promise<void> {
    const identity: BindIdentity = {
      project_root: canonicalizeProjectRoot(projectRoot),
      harness: this.harness,
      session: session && session.length > 0 ? session : DEFAULT_SESSION_ID,
    };
    const owner = this.detachSession(identityKey(identity), "session_closed");
    if (!this.sessionIndex.has(identity.session)) {
      const state = this.hostScopeCache.get(identity.session);
      if (state) this.cancelHostScopeRetry(state);
      this.hostScopeCache.delete(identity.session);
      this.hostScopeRefusals.delete(identity.session);
    }
    if (owner) await this.cleanupDetached(owner);
  }
}

/**
 * Resolve a background nudge exactly once by looking up the existing active
 * bridge without creating one or reviving a shut-down pool; hosts use this
 * function to acknowledge nudge handling.
 */
export function resolveBridgeForNudge(
  pool: AftTransportPool,
  ref: BgNudgeRef,
): AftProjectTransport {
  const candidate = pool as AftTransportPool & {
    getActiveBridgeForRootGeneration?: (value: BgNudgeRef) => AftProjectTransport | null;
    recordBgNudgeRejection?: (value: BgNudgeRef) => void;
    getCurrentRootGeneration?: (root: CanonicalRootPath) => RootGeneration | undefined;
    getConcretePoolId?: () => ConcretePoolId | undefined;
  };
  const bridge = candidate.getActiveBridgeForRootGeneration?.(ref) ?? null;
  if (bridge) return bridge;

  candidate.recordBgNudgeRejection?.(ref);
  throw new SubcRootGenerationExpiredError({
    canonicalRoot: ref.canonicalRoot,
    expectedGeneration: ref.generation,
    currentGeneration: candidate.getCurrentRootGeneration?.(ref.canonicalRoot),
    concretePoolId: ref.concretePoolId,
    currentConcretePoolId: candidate.getConcretePoolId?.(),
  });
}
