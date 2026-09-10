import { warn } from "./active-logger.js";
import { canonicalizeProjectRoot } from "./project-identity.js";
import type { BgNudgeRef } from "./subc-transport.js";
import type {
  AftProjectTransport,
  AftTransportPool,
  ToolCallArguments,
  ToolCallOptions,
  ToolCallResult,
} from "./transport.js";

interface StatusSubscribableBridge {
  subscribeStatus(listener: (snapshot: Record<string, unknown>) => void): () => void;
}

type StatusListener = (snapshot: Record<string, unknown>) => void;

type PoolFactory = () => Promise<AftTransportPool>;

/** After a failed terminal-pool revival, wait before retrying so repeated traffic cannot immediately start another connection attempt. */
const REVIVAL_RETRY_FLOOR_MS = 100;
const REVIVAL_RETRY_CAP_MS = 2_000;

/**
 * Owns one terminal transport instance and replaces it when new demand arrives
 * after the host has shut it down. The replaced instance is never reused: its
 * routes, sessions, and sockets remain owned by the dead instance.
 */
export class RevivableTransportPool implements AftTransportPool {
  private activePool: AftTransportPool;
  private revival: Promise<AftTransportPool> | null = null;
  private revivalRetryDelayMs = REVIVAL_RETRY_FLOOR_MS;
  private revivalRetryNotBefore = 0;
  private shutdownReason: string | null = null;
  private readonly transports = new Map<string, RevivableProjectTransport>();
  private readonly configureOverrides = new Map<string, unknown>();
  private editSlotSurvivesCaptured = false;

  constructor(
    initialPool: AftTransportPool,
    private readonly createPool: PoolFactory,
    private readonly onBinaryReplaced?: (path: string) => void,
  ) {
    this.activePool = initialPool;
  }

  getBridge(projectRoot: string): RevivableProjectTransport {
    const key = canonicalizeProjectRoot(projectRoot);
    let transport = this.transports.get(key);
    if (!transport) {
      transport = new RevivableProjectTransport(this, key);
      this.transports.set(key, transport);
    }
    return transport;
  }

  /** Delegate to the active pool: session-scoped signal fan-out reaches the
   * same live transports the underlying pool would report. */
  activeBridges(): AftProjectTransport[] {
    return this.activePool.activeBridges();
  }

  getActiveBridgeForRoot(projectRoot: string): RevivableProjectTransport | null {
    const key = canonicalizeProjectRoot(projectRoot);
    const bridge = this.currentBridge(key);
    if (!bridge) return null;
    const transport = this.getBridge(key);
    transport.refreshStatusSubscription(bridge);
    return transport;
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

  setConfigureOverride(key: string, value: unknown): void {
    if (key === "edit_slot_survives") {
      if (typeof value !== "boolean") {
        throw new Error("edit_slot_survives must be set once to a boolean");
      }
      if (this.editSlotSurvivesCaptured) {
        throw new Error("edit_slot_survives is write-once and was already captured");
      }
      this.activePool.setConfigureOverride(key, value);
      this.editSlotSurvivesCaptured = true;
      this.configureOverrides.set(key, value);
      return;
    }

    if (value === undefined) this.configureOverrides.delete(key);
    else this.configureOverrides.set(key, value);
    this.activePool.setConfigureOverride(key, value);
  }

  async reconfigure(projectRoot: string, overrides: Record<string, unknown>): Promise<void> {
    const pool = await this.ensureActivePool();
    const runtimeOverrides: Record<string, unknown> = {};
    for (const [key, value] of Object.entries(overrides)) {
      if (key === "edit_slot_survives") {
        this.setConfigureOverride(key, value);
        continue;
      }
      if (value === undefined) this.configureOverrides.delete(key);
      else this.configureOverrides.set(key, value);
      runtimeOverrides[key] = value;
    }
    await pool.reconfigure(projectRoot, runtimeOverrides);
  }

  async replaceBinary(path: string): Promise<string> {
    const replaced = await this.activePool.replaceBinary(path);
    this.onBinaryReplaced?.(replaced);
    return replaced;
  }

  closeSession(projectRoot: string, session: string): Promise<void> {
    return this.activePool.closeSession(projectRoot, session);
  }

  async shutdown(reason = "unknown"): Promise<void> {
    const revival = this.revival;
    if (revival) {
      await Promise.allSettled([revival]);
    }
    this.shutdownReason = reason;
    await this.activePool.shutdown(reason);
    for (const transport of this.transports.values()) {
      transport.refreshStatusSubscription(null);
    }
  }

  isShutdown(): boolean {
    return this.activePool.isShutdown();
  }

  async ensureActivePool(): Promise<AftTransportPool> {
    if (!this.activePool.isShutdown()) return this.activePool;
    if (this.revival) return this.revival;

    const delay = Math.max(0, this.revivalRetryNotBefore - Date.now());
    if (delay > 0) {
      let scheduled!: Promise<AftTransportPool>;
      scheduled = new Promise<void>((resolve) => setTimeout(resolve, delay)).then(() => {
        if (this.revival === scheduled) this.revival = null;
        return this.ensureActivePool();
      });
      this.revival = scheduled;
      void scheduled.then(
        () => undefined,
        () => undefined,
      );
      return scheduled;
    }

    warn(
      `transport was shut down (reason: ${this.shutdownReason ?? "unknown"}) but new demand arrived — reviving`,
    );
    const revival = Promise.resolve()
      .then(() => this.createPool())
      .then((pool) => {
        for (const [key, value] of this.configureOverrides) {
          pool.setConfigureOverride(key, value);
        }
        this.activePool = pool;
        this.shutdownReason = null;
        this.revivalRetryDelayMs = REVIVAL_RETRY_FLOOR_MS;
        this.revivalRetryNotBefore = 0;
        for (const [root, transport] of this.transports) {
          transport.refreshStatusSubscription(pool.getActiveBridgeForRoot(root));
        }
        return pool;
      });
    this.revival = revival;
    revival.then(
      () => {
        if (this.revival === revival) this.revival = null;
      },
      () => {
        // A terminal pool is parked after a failed revival. Concurrent and newly
        // arriving calls share the one delayed retry instead of recursively
        // starting connections or registering fresh listeners on every call.
        this.revivalRetryNotBefore = Date.now() + this.revivalRetryDelayMs;
        this.revivalRetryDelayMs = Math.min(this.revivalRetryDelayMs * 2, REVIVAL_RETRY_CAP_MS);
        if (this.revival === revival) this.revival = null;
      },
    );
    return revival;
  }

  /**
   * Return only the existing outer facade that matches a live concrete nudge.
   * Unlike getActiveBridgeForRoot, this path never creates a wrapper facade or
   * asks the active pool for demand.
   */
  getActiveBridgeForRootGeneration(ref: BgNudgeRef): RevivableProjectTransport | null {
    const concretePool = this.activePool as AftTransportPool & {
      getActiveBridgeForRootGeneration?: (value: BgNudgeRef) => AftProjectTransport | null;
    };
    if (!concretePool.getActiveBridgeForRootGeneration?.(ref)) return null;
    return this.transports.get(ref.canonicalRoot) ?? null;
  }

  recordBgNudgeRejection(ref: BgNudgeRef): void {
    const pool = this.activePool as AftTransportPool & {
      recordBgNudgeRejection?: (value: BgNudgeRef) => void;
    };
    pool.recordBgNudgeRejection?.(ref);
  }

  getCurrentRootGeneration(root: string): BgNudgeRef["generation"] | undefined {
    const pool = this.activePool as AftTransportPool & {
      getCurrentRootGeneration?: (value: string) => BgNudgeRef["generation"] | undefined;
    };
    return pool.getCurrentRootGeneration?.(root);
  }

  getConcretePoolId(): BgNudgeRef["concretePoolId"] | undefined {
    const pool = this.activePool as AftTransportPool & {
      getConcretePoolId?: () => BgNudgeRef["concretePoolId"] | undefined;
    };
    return pool.getConcretePoolId?.();
  }

  currentBridge(projectRoot: string): AftProjectTransport | null {
    return this.activePool.getActiveBridgeForRoot(projectRoot);
  }

  async send(
    projectRoot: string,
    command: string,
    params?: Record<string, unknown>,
    options?: Parameters<AftProjectTransport["send"]>[2],
  ): Promise<Record<string, unknown>> {
    const pool = await this.ensureActivePool();
    const bridge = pool.getBridge(projectRoot);
    this.getBridge(projectRoot).refreshStatusSubscription(bridge);
    return bridge.send(command, params, options);
  }

  async toolCallOnProject(
    projectRoot: string,
    sessionId: string | undefined,
    name: string,
    rawArgs?: ToolCallArguments,
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    const pool = await this.ensureActivePool();
    const bridge = pool.getBridge(projectRoot);
    this.getBridge(projectRoot).refreshStatusSubscription(bridge);
    return bridge.toolCall(sessionId, name, rawArgs, options);
  }
}

/**
 * Stable project facade returned to callers so a facade acquired before a host
 * quit hook still routes the next call through the replacement pool.
 */
class RevivableProjectTransport implements AftProjectTransport {
  private readonly statusListeners = new Map<StatusListener, () => void>();
  private readonly statusBridges = new Map<StatusListener, AftProjectTransport | null>();

  constructor(
    private readonly owner: RevivableTransportPool,
    private readonly projectRoot: string,
  ) {}

  getCwd(): string {
    return this.projectRoot;
  }

  getCachedStatus() {
    return this.owner.currentBridge(this.projectRoot)?.getCachedStatus() ?? null;
  }

  cacheStatusSnapshot(snapshot: Parameters<AftProjectTransport["cacheStatusSnapshot"]>[0]): void {
    this.owner.currentBridge(this.projectRoot)?.cacheStatusSnapshot(snapshot);
  }

  send(
    command: string,
    params?: Record<string, unknown>,
    options?: Parameters<AftProjectTransport["send"]>[2],
  ): Promise<Record<string, unknown>> {
    return this.owner.send(this.projectRoot, command, params, options);
  }

  toolCall(
    sessionId: string | undefined,
    name: string,
    rawArgs?: ToolCallArguments,
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    return this.owner.toolCallOnProject(this.projectRoot, sessionId, name, rawArgs, options);
  }

  subscribeStatus(listener: StatusListener): () => void {
    if (this.statusListeners.has(listener)) return () => this.removeStatusListener(listener);
    this.statusListeners.set(listener, () => {});
    this.bindStatusListener(listener, this.owner.currentBridge(this.projectRoot));
    return () => this.removeStatusListener(listener);
  }

  refreshStatusSubscription(bridge: AftProjectTransport | null): void {
    for (const listener of this.statusListeners.keys()) {
      this.bindStatusListener(listener, bridge);
    }
  }

  private bindStatusListener(listener: StatusListener, bridge: AftProjectTransport | null): void {
    if (this.statusBridges.get(listener) === bridge) return;
    const previousUnsubscribe = this.statusListeners.get(listener);
    previousUnsubscribe?.();
    const maybe = bridge as (AftProjectTransport & Partial<StatusSubscribableBridge>) | null;
    const unsubscribe =
      maybe && typeof maybe.subscribeStatus === "function"
        ? maybe.subscribeStatus(listener)
        : () => {};
    this.statusListeners.set(listener, unsubscribe);
    this.statusBridges.set(listener, bridge);
  }

  private removeStatusListener(listener: StatusListener): void {
    const unsubscribe = this.statusListeners.get(listener);
    this.statusListeners.delete(listener);
    this.statusBridges.delete(listener);
    unsubscribe?.();
  }
}
