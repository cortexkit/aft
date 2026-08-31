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

import { existsSync, statSync } from "node:fs";

import type { RouteHandle } from "@cortexkit/subc-client";
import {
  type BindIdentity,
  type ConsumerIdentity,
  connectionFileExists,
  isConsumerReconnectTransient,
  type RequestOptions,
  type RouteTarget,
  StaleRouteHandleError,
  SubcCallError,
  SubcClient,
  SubcError,
} from "@cortexkit/subc-client";

import { log } from "./active-logger.js";
import type { StatusSnapshot } from "./bridge.js";
import { isRouteOpenReloadWindowError } from "./error-contract.js";
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

/** The subc pool is closing and cannot carry another request. */
export class SubcTransportShuttingDownError extends SubcCallError {
  constructor() {
    super("terminal", "subc transport is shutting down", "transport_shutting_down");
    this.name = "SubcTransportShuttingDownError";
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
    opts?: { consumerIdentity?: ConsumerIdentity | null },
  ): Promise<RouteHandle>;
  request(route: RouteHandle, body: unknown, opts?: RequestOptions): Promise<unknown>;
  subscribe(
    route: RouteHandle,
    body: unknown,
    onEvent: (event: Uint8Array) => void,
  ): SubcSubscriptionLike;
  closeRouteChannel(route: RouteHandle, opts?: { drain?: boolean }): Promise<void>;
  /** Cumulative frames discarded because their route epoch did not match the client's current handle. */
  readonly droppedIngressFrames?: number;
  close(): void;
}

/** The subc module id AFT registers under (matches the daemon manifest). */
const AFT_MODULE_ID = "aft";

/**
 * A run of consecutive NON-transient transport throws (timeout / route GOODBYE)
 * on the SAME client is presumed a dead half-open connection (local writes
 * succeed, no response ever arrives), so the client is dropped after this many.
 * A single throw does not drop the client (a slow tool can legitimately time out
 * once); the counter resets on any successful request. Tool-level errors never
 * count — they return `success:false`, they do not throw. (Audit B-#4.)
 */
const MAX_CONSECUTIVE_TRANSPORT_FAILURES = 3;

/** Every transport reconnect waits at least one event-loop turn and caps repeated failures. */
const RECONNECT_RETRY_FLOOR_MS = 100;
const RECONNECT_RETRY_CAP_MS = 2_000;
/** Maximum total restart-burst delay a fresh route.open may absorb. */
const ROUTE_OPEN_RELOAD_WAIT_CAP_MS = 15_000;
const ROUTE_OPEN_RELOAD_WAIT_EXHAUSTED_SUFFIX =
  " The AFT daemon module did not return within the 15s reload window.";

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
  /** Handshake timeout forwarded to SubcClient.connect. */
  handshakeTimeoutMs?: number;
  /**
   * Pool default request budget in milliseconds. When neither the caller nor
   * the tool adapter supplies a timeout, every route request derives one
   * absolute deadline from this value at entry and never restarts it across
   * connection, route-open, backoff, or stale-route retry. A direct pool that
   * omits this (tests) carries no deadline metadata.
   */
  defaultTimeoutMs?: number;
  /**
   * Connection factory seam. Defaults to the real `SubcClient.connect`. Tests
   * inject a fake to exercise route caching / Rd reconnect without a daemon.
   */
  connect?: (opts: {
    connectionFile: string;
    handshakeTimeoutMs?: number;
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
  private readonly loop: Promise<void>;
  private readonly lifecycleLogState = new Map<
    string,
    { lastEmittedAt: number; suppressed: number }
  >();
  private nudgeReceiptLogState: { lastEmittedAt: number; count: number } | null = null;

  constructor(
    private readonly identity: BindIdentity,
    private readonly acquireClient: () => Promise<SubcClientLike>,
    private readonly dropClient: (client: SubcClientLike) => void,
    private readonly consumerIdentity: ConsumerIdentity | null | undefined,
    private readonly onNudge: () => void,
    private readonly sleep: (ms: number) => Promise<void>,
    private readonly canAttach: () => boolean,
    private readonly onRootAttachFailure: (error: unknown) => boolean,
    private readonly onDormant: () => void,
    private readonly dispatchProbeIntervalMs: number,
    readonly nudgeRef?: BgNudgeRef,
    private readonly isCurrent: () => boolean = () => true,
  ) {
    this.loop = this.run();
  }

  async stop(): Promise<void> {
    this.stopped = true;
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
    timer.unref?.();
    return () => clearInterval(timer);
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
        route = await client.routeOpen(
          { kind: "tool_provider", module_id: AFT_MODULE_ID },
          this.identity,
          { consumerIdentity: this.consumerIdentity },
        );
      } catch (err) {
        if (this.isCurrent() && this.onRootAttachFailure(err)) {
          if (reconnecting) giveUp("root-dormant");
          this.onDormant();
          return;
        }
        if (isConsumerReconnectTransient(err)) this.dropClient(client);
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
          if (!this.isCurrent()) {
            this.info(
              "nudge-stale-carrier",
              `nudge carried by stale subscription; checking current session channel=${routeId}`,
            );
          }
          this.onNudge();
        });
        this.current = sub;
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
        if (!this.stopped && this.isCurrent()) this.onNudge();

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
        if (isConsumerReconnectTransient(err)) this.dropClient(client);
        if (Date.now() - subscribedAt >= BG_STABLE_MS) backoffAttempt = 0;
        beginReconnect();
      } finally {
        stopDispatchProbe();
        this.current = null;
        safeCloseRoute(client, route);
      }
      await this.backoff(backoffAttempt++);
    }

    if (reconnecting) giveUp("stopped");
  }

  private async backoff(attempt: number): Promise<void> {
    await this.sleep(reconnectBackoffMs(attempt));
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isRouteProvenAbsentError(err: unknown): boolean {
  return (
    (err instanceof SubcError && err.code === "unknown_channel") ||
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
function reloadWindowExhaustedError(error: unknown): unknown {
  if (error instanceof Error) {
    try {
      error.message += ROUTE_OPEN_RELOAD_WAIT_EXHAUSTED_SUFFIX;
      return error;
    } catch {
      // A frozen SDK error cannot carry the suffix; retain its message in a wrapper.
    }
  }
  return new Error(`${String(error)}${ROUTE_OPEN_RELOAD_WAIT_EXHAUSTED_SUFFIX}`);
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

/** Per-identity session lifecycle state, independent from transient route churn. */
interface SessionRecord {
  /** Complete identity is retained; the opaque map key is never parsed. */
  readonly identity: BindIdentity;
  readonly identityKey: IdentityKey;
  readonly canonicalRoot: CanonicalRootPath;
  readonly generation?: RootGeneration;
  /** Current tool route for this session incarnation; replaced after route failures. */
  routeEntry: RouteEntry | null;
  /** Dedicated bg_events subscription, present only when background events are enabled. */
  bgSub: BgSubscription | null;
  /** Closed marker set synchronously so in-flight requests can see the close. */
  closed: boolean;
  /** Root close marks this before any asynchronous cleanup starts. */
  teardownReason: "root_reaped" | "session_closed" | "shutdown" | null;
  /** Count of in-flight requests on this session's route; used for safe cleanup. */
  inflight: number;
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
}

/**
 * A route open that resolved AFTER its session was torn down (closeSession /
 * shutdown / client swap). The caller's request can't proceed, but this is an
 * intentional teardown — NOT a transport fault — so it must not drop the client
 * or count toward the half-open-socket failure budget (B-#3/B-#4).
 */
class RouteTornDownError extends Error {}

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
    const { preview, timeoutMs, executionDeadlineMs, onProgress } = this.splitOptions(options);
    const body: Record<string, unknown> = { name, arguments: rawArgs };
    const editSlotSurvives = this.pool.getEditSlotSurvives();
    if (editSlotSurvives !== undefined) body.edit_slot_survives = editSlotSurvives;
    if (preview === true) body.preview = true;
    if (executionDeadlineMs !== undefined) body.deadline_ms_remaining = executionDeadlineMs;
    const reply = await this.pool.routeRequest(
      this.identityFor(sessionId),
      body,
      timeoutMs,
      onProgress,
      this.generation,
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
    const { timeoutMs, onProgress } = this.splitOptions(options);
    const session = typeof params.session_id === "string" ? params.session_id : undefined;
    const body: Record<string, unknown> = { name: command, arguments: params };
    const editSlotSurvives = this.pool.getEditSlotSurvives();
    if (editSlotSurvives !== undefined) body.edit_slot_survives = editSlotSurvives;
    const reply = await this.pool.routeRequest(
      this.identityFor(session),
      body,
      timeoutMs,
      onProgress,
      this.generation,
    );
    return reliftReply(reply);
  }

  private splitOptions(options?: ToolCallOptions): {
    preview?: boolean;
    timeoutMs?: number;
    executionDeadlineMs?: number;
    onProgress?: RequestOptions["onProgress"];
  } {
    if (!options) return {};
    const preview = (options as ToolCallOptions).preview;
    // Mirror BinaryBridge.send's budget precedence (transportTimeoutMs wins):
    // orchestrated bash passes its wait-aware budget as transportTimeoutMs, and
    // dropping it here would cap long tool executions at the subc client's
    // default unary deadline while the command keeps running module-side.
    // The pool default budget applies when the caller supplied neither.
    const timeoutMs =
      options.transportTimeoutMs ?? options.timeoutMs ?? this.pool.poolDefaultTimeoutMs;
    const executionDeadlineMs = options.executionDeadlineMs;
    const onProgress = options.onProgress
      ? (body: Uint8Array) =>
          options.onProgress?.({ kind: "stdout", text: new TextDecoder().decode(body) })
      : undefined;
    return { preview, timeoutMs, executionDeadlineMs, onProgress };
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
  private readonly defaultTimeoutMs?: number;
  private readonly consumerIdentity: ConsumerIdentity | null | undefined;
  private readonly connectFn: (opts: {
    connectionFile: string;
    handshakeTimeoutMs?: number;
  }) => Promise<SubcClientLike>;

  private readonly onBgEventsNudge?: (projectRoot: string, session: string) => void;
  private readonly onBgEventsNudgeRef?: (ref: BgNudgeRef) => void;
  private readonly bgBackoffSleep: (ms: number) => Promise<void>;
  private readonly routeRetrySleep: (ms: number) => Promise<void>;
  private readonly bgDispatchProbeIntervalMs: number;
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
  /**
   * The sole root-scoped session enumeration authority. Keys are opaque and are
   * removed by the same detacher that removes the corresponding session record.
   */
  private readonly rootIndex = new Map<CanonicalRootPath, Set<IdentityKey>>();
  /** Roots whose route binds must stay dormant until their directories return. */
  private readonly dormantRoots = new Map<CanonicalRootPath, SubcError>();
  /** Consecutive non-transient failures on the current pool-local client. */
  private transportFailures = 0;
  /** Concrete per-root facades, including their captured root generation. */
  private readonly transports = new Map<CanonicalRootPath, SubcTransport>();
  private readonly generationRejections = new Set<string>();
  private readonly nudgeDeliveryLogState = new Map<
    string,
    { lastEmittedAt: number; suppressed: number }
  >();
  private readonly pendingRootCleanups = new Set<Promise<unknown>>();
  private shuttingDown = false;
  private editSlotSurvives: boolean | undefined;
  private editSlotSurvivesCaptured = false;

  constructor(options: SubcTransportPoolOptions) {
    this.connectionFile = options.connectionFile;
    this.harness = options.harness;
    this.handshakeTimeoutMs = options.handshakeTimeoutMs;
    this.defaultTimeoutMs = options.defaultTimeoutMs;
    this.consumerIdentity = options.consumerIdentity;
    this.connectFn = options.connect ?? ((opts) => SubcClient.connect(opts));
    this.onBgEventsNudge = options.onBgEventsNudge;
    this.onBgEventsNudgeRef = options.onBgEventsNudgeRef;
    this.bgBackoffSleep =
      options.bgBackoffSleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms)));
    this.routeRetrySleep =
      options.routeRetrySleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms)));
    this.bgDispatchProbeIntervalMs =
      options.bgDispatchProbeIntervalMs ?? BG_DISPATCH_PROBE_INTERVAL_MS;
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

  /** Pool default request budget; `undefined` means no deadline metadata. */
  get poolDefaultTimeoutMs(): number | undefined {
    return this.defaultTimeoutMs;
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
   * Check a root immediately before opening a route. The reclaim marker is only
   * an existence hint; a directory that has already returned wins over a stale
   * sibling marker, so the marker contents are never parsed.
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

    // Read the marker only as a latency hint. Directory absence is sufficient
    // to suspend the bind, and the marker's JSON is intentionally irrelevant.
    const reclaimedMarkerPresent = existsSync(`${root}.reclaimed`);
    if (reclaimedMarkerPresent) {
      this.markRootDormant(root);
      return false;
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
    const root = this.canonicalRoot(projectRoot);
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
      return record;
    }
    record = {
      identity,
      identityKey: key,
      canonicalRoot: root,
      generation,
      routeEntry: null,
      bgSub: null,
      closed: false,
      teardownReason: null,
      inflight: 0,
    };
    this.sessions.set(key, record);
    let keys = this.rootIndex.get(root);
    if (!keys) {
      keys = new Set<IdentityKey>();
      this.rootIndex.set(root, keys);
    }
    keys.add(key);
    return record;
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
    const key = `${kind}\u0000${record.identityKey}`;
    const now = Date.now();
    const state = this.nudgeDeliveryLogState.get(key);
    if (state && now - state.lastEmittedAt < BG_LIFECYCLE_LOG_INTERVAL_MS) {
      state.suppressed += 1;
      return;
    }
    const suppressed = state?.suppressed ?? 0;
    this.nudgeDeliveryLogState.set(key, { lastEmittedAt: now, suppressed: 0 });
    const suffix = suppressed > 0 ? ` suppressed=${suppressed}` : "";
    log(`subc bg_events: ${message}${suffix}`, { sessionId: record.identity.session });
  }

  private removeIndexMembership(record: SessionRecord): void {
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
      record.routeEntry === null &&
      record.bgSub === null
    ) {
      this.sessions.delete(record.identityKey);
      this.removeIndexMembership(record);
    }
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

    const bgSub = record.bgSub;
    record.bgSub = null;
    const routeEntry = record.routeEntry;
    record.routeEntry = null;
    if (routeEntry) routeEntry.closed = true;
    return { record, bgSub, routeEntry };
  }

  private async cleanupDetached(detached: DetachedSession): Promise<void> {
    const cleanup: Promise<unknown>[] = [];
    if (detached.bgSub) cleanup.push(detached.bgSub.stop());
    const routeEntry = detached.routeEntry;
    const route = routeEntry?.handle;
    if (routeEntry && route !== null && route !== undefined) {
      try {
        cleanup.push(
          Promise.resolve(routeEntry.client.closeRouteChannel(route)).catch(() => undefined),
        );
      } catch {
        // A client may throw synchronously when its socket is already closed.
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

  /** Race a shared wait against THIS caller's remaining request budget.
   *
   * The underlying promise (shared connect, shared route opening, pooled
   * backoff timer) is NEVER cancelled or invalidated by one caller's expiry:
   * late settlement is observed via a detached handler so it can still cache
   * the client/route, and an unhandled rejection cannot occur.
   */
  private awaitWithinRequestBudget(
    wait: Promise<unknown>,
    remaining: number | undefined,
    phase: string,
  ): Promise<unknown> {
    if (remaining === undefined || !Number.isFinite(remaining)) return wait;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const timeoutPromise = new Promise<never>((_, reject) => {
      timer = setTimeout(
        () =>
          reject(
            new SubcCallError(
              "not_sent",
              `request deadline elapsed while waiting for ${phase}`,
              "request_deadline_exceeded_before_send",
            ),
          ),
        Math.max(0, Math.ceil(remaining)),
      );
    });
    return Promise.race([wait, timeoutPromise]).finally(() => {
      clearTimeout(timer);
      // Observe late settlement of the shared wait: the loser of the race must
      // neither cache nothing nor surface an unhandled rejection. The shared
      // client/route itself is never invalidated by this caller's expiry.
      wait.then(
        () => undefined,
        () => undefined,
      );
    });
  }

  /**
   * Open or reuse a route while guarding every lifecycle boundary.
   *
   * One absolute local request deadline is derived at entry from the exact
   * precedence `transportTimeoutMs ?? timeoutMs ?? pool.defaultTimeoutMs` and
   * never restarts: connection, route open, reload-window backoff, stale-route
   * retry backoff, and the request itself all draw down the same budget.
   * Immediately before each `client.request` attempt the remaining budget is
   * recomputed and stamped as top-level `deadline_ms_remaining` on the request
   * body (arguments are never mutated), and the same remaining value becomes
   * the request's `RequestOptions.timeoutMs`.
   */
  async routeRequest(
    identity: BindIdentity,
    body: Record<string, unknown>,
    timeoutMs?: number,
    onProgress?: RequestOptions["onProgress"],
    expectedGeneration?: RootGeneration,
  ): Promise<unknown> {
    const effectiveTimeoutMs = timeoutMs ?? this.defaultTimeoutMs;
    const deadlineMs = Number.isFinite(effectiveTimeoutMs)
      ? Date.now() + (effectiveTimeoutMs as number)
      : undefined;
    const remainingMs = (): number | undefined =>
      deadlineMs === undefined ? undefined : deadlineMs - Date.now();

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
        client = (await this.awaitWithinRequestBudget(
          this.ensureClient(),
          remainingMs(),
          "connection",
        )) as SubcClientLike;
        this.assertRecordLive(record);
      } catch (error) {
        throw this.annotateReapError(error, record);
      }

      const openRoute = async (): Promise<{ route: RouteHandle; entry: RouteEntry }> => {
        try {
          this.assertRecordLive(record);
          const opened = (await this.awaitWithinRequestBudget(
            this.routeHandle(client, identity, record),
            remainingMs(),
            "route-open",
          )) as { route: RouteHandle; entry: RouteEntry };
          this.assertRecordLive(record);
          return opened;
        } catch (error) {
          if (this.isReapInduced(record)) throw this.annotateReapError(error, record);
          if (error instanceof RouteTornDownError) throw error;
          if (error instanceof SubcCallError && error.kind === "not_sent") {
            // Caller-scoped budget expiry: the shared connect/open is healthy
            // and must survive for other callers, so it is never dropped here.
            throw error;
          }
          if (
            isConsumerReconnectTransient(error) &&
            this.isCurrentSession(key, record) &&
            this.client === client
          ) {
            this.dropClient(client);
          }
          throw error;
        }
      };

      const openRouteAfterReloadWindow = async (): Promise<{
        route: RouteHandle;
        entry: RouteEntry;
      }> => {
        let reloadWaitedMs = 0;
        while (true) {
          try {
            return await openRoute();
          } catch (error) {
            if (!isRouteOpenReloadWindowError(error)) throw error;
            const { delayMs, wait } = this.waitForRouteReopenBackoff();
            if (reloadWaitedMs + delayMs > ROUTE_OPEN_RELOAD_WAIT_CAP_MS) {
              throw reloadWindowExhaustedError(error);
            }
            reloadWaitedMs += delayMs;
            await this.awaitWithinRequestBudget(wait, remainingMs(), "reload-window");
          }
        }
      };

      const clearRouteEntry = (entry: RouteEntry): void => {
        if (record.routeEntry !== entry) return;
        entry.closed = true;
        record.routeEntry = null;
        if (entry.handle != null) safeCloseRoute(entry.client, entry.handle);
      };

      const handleRequestFailure = (error: unknown, entry: RouteEntry): void => {
        clearRouteEntry(entry);
        if (
          !this.isCurrentSession(key, record) ||
          this.client !== client ||
          this.isReapInduced(record)
        )
          return;
        if (isConsumerReconnectTransient(error)) {
          this.transportFailures = 0;
          this.dropClient(client);
          return;
        }
        if (
          !isRouteProvenAbsentError(error) &&
          ++this.transportFailures >= MAX_CONSECUTIVE_TRANSPORT_FAILURES
        ) {
          this.transportFailures = 0;
          this.dropClient(client);
        }
      };

      const requestOnRoute = async (route: RouteHandle): Promise<unknown> => {
        this.assertRecordLive(record);
        // Immediately before bytes go on the wire: recompute the remaining
        // budget from the unchanged absolute deadline. If none remains, the
        // request is PROVABLY not sent.
        const remaining = remainingMs();
        if (remaining !== undefined && remaining <= 0) {
          throw new SubcCallError(
            "not_sent",
            "request deadline elapsed before the request could be sent",
            "request_deadline_exceeded_before_send",
          );
        }
        const requestTimeoutMs =
          remaining !== undefined ? Math.max(1, Math.floor(remaining)) : timeoutMs;
        const requestedExecutionDeadline = body.deadline_ms_remaining;
        const serverDeadline =
          typeof requestedExecutionDeadline === "number" &&
          Number.isFinite(requestedExecutionDeadline)
            ? Math.min(remaining ?? requestedExecutionDeadline, requestedExecutionDeadline)
            : remaining;
        const deadlineBody =
          serverDeadline === undefined || !Number.isFinite(serverDeadline)
            ? body
            : { ...body, deadline_ms_remaining: Math.max(0, Math.floor(serverDeadline)) };
        const reply = await client.request(route, deadlineBody, {
          timeoutMs: requestTimeoutMs,
          onProgress,
        });
        // A legacy closeSession may intentionally let an already-delivered reply
        // settle. It must not mutate shared state or recreate a subscription.
        if (!this.isCurrentSession(key, record)) {
          if (this.isReapInduced(record)) throw this.rootReapedError(record);
          return reply;
        }
        this.assertGeneration(record.canonicalRoot, record.generation, "request_completion");
        if (this.client === client) this.transportFailures = 0;
        this.ensureBgSubscription(identity, record);
        return reply;
      };

      let routeAndEntry = await openRouteAfterReloadWindow();
      try {
        return await requestOnRoute(routeAndEntry.route);
      } catch (error) {
        if (this.isReapInduced(record)) throw this.annotateReapError(error, record);
        if (error instanceof SubcCallError && error.kind === "not_sent") {
          // Caller-scoped budget expiry: the request provably never went out,
          // so the shared client/route state stays untouched for other callers.
          throw error;
        }
        if (
          isRouteProvenAbsentError(error) &&
          this.isCurrentSession(key, record) &&
          this.client === client
        ) {
          clearRouteEntry(routeAndEntry.entry);
          await this.awaitWithinRequestBudget(
            this.waitForRouteReopenBackoff().wait,
            remainingMs(),
            "stale-route-backoff",
          );
          routeAndEntry = await openRouteAfterReloadWindow();
          try {
            const reply = await requestOnRoute(routeAndEntry.route);
            this.resetRouteReopenBackoff();
            return reply;
          } catch (retryError) {
            if (this.isReapInduced(record)) throw this.annotateReapError(retryError, record);
            handleRequestFailure(retryError, routeAndEntry.entry);
            throw retryError;
          }
        }
        handleRequestFailure(error, routeAndEntry.entry);
        throw error;
      }
    } catch (error) {
      if (this.isReapInduced(record)) throw this.annotateReapError(error, record);
      throw error;
    } finally {
      record.inflight -= 1;
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

  private resetRouteReopenBackoff(): void {
    this.routeReopenRetryDelayMs = RECONNECT_RETRY_FLOOR_MS;
  }

  private async ensureClient(): Promise<SubcClientLike> {
    if (this.shuttingDown) throw new SubcTransportShuttingDownError();
    if (this.client) return this.client;
    if (this.connecting) return this.connecting;
    this.connecting = this.connectFn({
      connectionFile: this.connectionFile,
      handshakeTimeoutMs: this.handshakeTimeoutMs,
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
        this.transportFailures = 0;
        return client;
      })
      .catch((error) => {
        this.connecting = null;
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
    };
    this.assertRootCanAttach(record.canonicalRoot);
    const opening = client
      .routeOpen({ kind: "tool_provider", module_id: AFT_MODULE_ID }, identity, {
        consumerIdentity: this.consumerIdentity,
      })
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
        return route;
      })
      .catch((error) => {
        const currentEntry =
          this.isCurrentSession(record.identityKey, record) && record.routeEntry === entry;
        if (record.routeEntry === entry) {
          entry.closed = true;
          record.routeEntry = null;
        }
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

  private ensureBgSubscription(identity: BindIdentity, record: SessionRecord): void {
    if (this.shuttingDown || (!this.onBgEventsNudge && !this.onBgEventsNudgeRef)) return;
    if (!this.isCurrentSession(record.identityKey, record)) return;
    if (!this.rootCanAttach(record.canonicalRoot)) return;
    if (record.bgSub) return;

    const nudgeRef = this.nudgeRefFor(record);
    const onNudge = (): void => {
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

      const currentRef = this.nudgeRefFor(currentRecord);
      let delivered = false;
      if (currentRef && this.onBgEventsNudgeRef) {
        this.onBgEventsNudgeRef(currentRef);
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
        this.onBgEventsNudge(currentRecord.identity.project_root, currentRecord.identity.session);
        delivered = true;
      }
      if (delivered) return;

      this.logNudgeDelivery(
        "drop-no-compatible-handler",
        currentRecord,
        `nudge dropped cause=generation-provenance-unavailable-and-root-session-handler-unwired root=${currentRecord.canonicalRoot}`,
      );
    };
    let sub: BgSubscription | null = null;
    const clearDormantSubscription = (): void => {
      if (sub && record.bgSub === sub) record.bgSub = null;
    };
    sub = new BgSubscription(
      identity,
      () => this.ensureClient(),
      (client) => this.dropClient(client),
      this.consumerIdentity,
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
   */
  private dropClient(client: SubcClientLike): void {
    if (this.client !== client) return;
    this.client = null;
    for (const record of this.sessions.values()) {
      const entry = record.routeEntry;
      if (entry?.client !== client) continue;
      entry.closed = true;
      record.routeEntry = null;
      this.deleteSessionIfEmpty(record.identityKey, record);
    }
    this.transportFailures = 0;
    try {
      client.close();
    } catch {
      // A dead socket is already released by the peer.
    }
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
    this.dormantRoots.clear();

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
    this.transports.clear();

    const client = this.client;
    this.client = null;
    const pending = [...this.pendingRootCleanups];
    await Promise.allSettled([...detached.map((owner) => this.cleanupDetached(owner)), ...pending]);
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
