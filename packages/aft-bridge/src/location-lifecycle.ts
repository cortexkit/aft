import { AsyncLocalStorage } from "node:async_hooks";
import {
  type BridgeClock,
  BridgeTransportUnavailableError,
  BridgeTransportUnknownOutcomeError,
  type VersionMismatchCallback,
} from "./bridge.js";
import { AftModuleUnavailableError } from "./error-contract.js";
import type { BridgeToolCallRuntime } from "./pool.js";
import { canonicalizeProjectRoot } from "./project-identity.js";
import type {
  AftProjectTransport,
  AftTransportPool,
  ToolCallArguments,
  ToolCallOptions,
  ToolCallResult,
} from "./transport.js";
import { type AftTransportFactoryOptions, createAftTransportPool } from "./transport-factory.js";

const LIFECYCLE_STATE: unique symbol = Symbol.for(
  "@cortexkit/aft-bridge/location-lifecycle/v1",
) as never;
const RELEASE_LOCATION: unique symbol = Symbol.for(
  "@cortexkit/aft-bridge/location-release/v1",
) as never;
const EXECUTE_LOCATION: unique symbol = Symbol.for(
  "@cortexkit/aft-bridge/location-execute/v1",
) as never;
const OWNER_LOCK = "process-global-owner";
const EXECUTION_DRAIN_TIMEOUT_MS = 5_000;

export const LOCATION_RELOADED_NOT_SENT =
  "This call was not sent because the AFT plugin reloaded; retry it.";

type ExecutionClock = Pick<BridgeClock, "setTimeout" | "clearTimeout">;
const EXECUTION_CLOCK: ExecutionClock = {
  setTimeout: (callback, ms) => setTimeout(callback, ms),
  clearTimeout: (timer) => clearTimeout(timer as ReturnType<typeof setTimeout>),
};

type TransportMode = "standalone" | "subc";
type PoolFactory = (options: AftTransportFactoryOptions) => Promise<AftTransportPool>;

export interface AcquireBridgeDependencies {
  /** Construction seam for lifecycle tests. Production always uses createAftTransportPool. */
  createPool?: PoolFactory;
  /** Bounded finalizer drain; the clock seam lets tests expire it without sleeping. */
  executionDrainTimeoutMs?: number;
  executionClock?: ExecutionClock;
}

export interface BridgeLifecycleTopology {
  /** Live standalone daemon processes held by this JavaScript process (zero or one). */
  daemonProcesses: number;
  /** Location-scoped routes. A route exists for every acquired Location lease. */
  routes: number;
  /** Plugin-owned subc clients. The externally supervised daemon is not included. */
  subcClients: number;
  /** All currently acquired V2 Location leases. */
  locations: number;
}

export interface BridgeLifecycleCensus {
  watchers: number;
  listenPorts: number;
  routes: number;
  lspChildren: number;
  daemonProcesses: number;
}

export interface BridgeLifecycleCensusOptions {
  /** Delay before sampling. Leak probes use the required 2,000 ms settle. */
  settleMs?: number;
  abortSignal?: AbortSignal;
}

interface StandaloneOwner {
  readonly pool: AftTransportPool;
  readonly configByRoot: Map<string, Record<string, unknown>>;
  /** Includes admitted calls on retired leases until those calls settle. */
  executionInterrupts?: Set<() => void>;
  /**
   * Each live Location's own version-mismatch handler, in acquisition order.
   * The pool is built once with the first Location's options, but that
   * handler closes over the first Location's lease, which throws once that
   * Location is released while others still use the pool. The pool therefore
   * calls a live Location's handler instead (see `acquireBridge`). Absent on
   * an owner created by an older copy of this package loaded in the same
   * process, since the owner lives on `globalThis`.
   */
  readonly versionMismatchHandlers?: Map<LocationBridgeLease, VersionMismatchCallback>;
  readonly referencesByRoot: Map<string, number>;
  references: number;
}

interface LifecycleState {
  standalone: StandaloneOwner | null;
  readonly subcLeases: Set<LocationBridgeLease>;
  readonly locks: Map<string, Promise<void>>;
}

type LifecycleGlobal = typeof globalThis & {
  [LIFECYCLE_STATE]?: LifecycleState;
};

interface ReleasableLocationPool extends AftTransportPool {
  [RELEASE_LOCATION](): Promise<void>;
  [EXECUTE_LOCATION]<T>(operation: () => Promise<T>): Promise<T>;
}

interface ExecutionHold {
  dispatched: boolean;
  finished: boolean;
  taskId?: string;
  error?: Error;
  interrupt(error: Error): void;
}

function lifecycleState(): LifecycleState {
  const host = globalThis as LifecycleGlobal;
  return (host[LIFECYCLE_STATE] ??= {
    standalone: null,
    subcLeases: new Set(),
    locks: new Map(),
  });
}

function modeFor(options: AftTransportFactoryOptions): TransportMode {
  return options.subcConnectionFile?.trim() ? "subc" : "standalone";
}

async function withLock<T>(key: string, operation: () => Promise<T>): Promise<T> {
  const locks = lifecycleState().locks;
  const previous = locks.get(key) ?? Promise.resolve();
  let unlock!: () => void;
  const current = new Promise<void>((resolve) => {
    unlock = resolve;
  });
  const tail = previous.catch(() => undefined).then(() => current);
  locks.set(key, tail);

  await previous.catch(() => undefined);
  try {
    return await operation();
  } finally {
    unlock();
    if (locks.get(key) === tail) locks.delete(key);
  }
}

function assertLive(released: boolean): void {
  if (released) {
    throw new BridgeTransportUnavailableError(
      "AFT bridge Location lease has already been released",
    );
  }
}

/**
 * A Location receives its own idempotent lease even when it shares the standalone
 * owner. This prevents one finalizer from shutting down resources still used by a
 * sibling Location and makes duplicate host disposal harmless.
 */
class LocationBridgeLease implements ReleasableLocationPool {
  private released = false;
  private releasePromise: Promise<void> | null = null;
  private readonly execution = new AsyncLocalStorage<ExecutionHold>();
  private readonly holds = new Set<ExecutionHold>();
  private drained?: () => void;

  constructor(
    readonly directory: string,
    readonly mode: TransportMode,
    private readonly pool: AftTransportPool,
    private readonly releaseOwner: () => Promise<void>,
    private readonly ownerExecutions: Set<() => void>,
    private readonly dependencies: AcquireBridgeDependencies,
  ) {}

  [EXECUTE_LOCATION]<T>(operation: () => Promise<T>): Promise<T> {
    if (this.released) return Promise.reject(this.notSent());
    let interrupt!: (error: Error) => void;
    const interrupted = new Promise<never>((_resolve, reject) => {
      interrupt = reject;
    });
    const hold: ExecutionHold = { dispatched: false, finished: false, interrupt };
    const interruptForShutdown = () => {
      hold.error ??= this.interrupted(hold);
      hold.interrupt(hold.error);
    };
    this.ownerExecutions.add(interruptForShutdown);
    this.holds.add(hold);
    // Admission is synchronous. Async-local ownership follows the whole invocation,
    // including permission waits and RPCs made after an earlier mutation answered.
    return this.execution.run(hold, async () => {
      try {
        return await Promise.race([operation(), interrupted]);
      } finally {
        hold.finished = true;
        this.ownerExecutions.delete(interruptForShutdown);
        this.holds.delete(hold);
        if (this.holds.size === 0) this.drained?.();
      }
    });
  }

  private notSent(): Error {
    return new AftModuleUnavailableError(LOCATION_RELOADED_NOT_SENT, undefined);
  }

  private interrupted(hold: ExecutionHold): Error {
    return hold.dispatched
      ? new BridgeTransportUnknownOutcomeError(
          `The AFT plugin reloaded before this invocation finished, so its outcome is UNKNOWN: it may or may not have executed.${
            hold.taskId ? ` Background task: ${hold.taskId}.` : ""
          } Verify actual state before re-running, and never blind-retry a mutation.`,
        )
      : this.notSent();
  }

  private currentHold(): ExecutionHold | undefined {
    const hold = this.execution.getStore();
    // Transport event sources can inherit a call's async context and outlive its
    // reply. Unsolicited events no longer belong to a completed call, but an
    // interrupted call must still fence its late continuations.
    return hold && (!hold.finished || hold.error) ? hold : undefined;
  }

  private assertUsable(hold = this.currentHold()): void {
    if (hold?.error) throw hold.error;
    if (hold?.finished) throw this.interrupted(hold);
    assertLive(this.released && !hold);
  }

  private dispatch<T>(operation: () => Promise<T>, hold = this.currentHold()): Promise<T> {
    this.assertUsable(hold);
    // Once an RPC is handed to a transport, teardown cannot prove it did not run.
    if (hold) hold.dispatched = true;
    return operation().then((result) => {
      if (hold && result && typeof result === "object" && "task_id" in result) {
        if (typeof result.task_id === "string") hold.taskId = result.task_id;
      }
      return result;
    });
  }

  private transport(bridge: AftProjectTransport): AftProjectTransport {
    const hold = this.currentHold();
    // Guard escaped transports too: getBridge may precede a permission wait.
    return new Proxy(bridge, {
      get: (target, key) => {
        if (key === "send") {
          return (...args: Parameters<AftProjectTransport["send"]>) =>
            this.dispatch(() => target.send(...args), hold);
        }
        if (key === "toolCall") {
          return (...args: Parameters<AftProjectTransport["toolCall"]>) =>
            this.dispatch(() => target.toolCall(...args), hold);
        }
        const value = Reflect.get(target, key, target);
        return typeof value === "function" ? value.bind(target) : value;
      },
    });
  }

  getBridge(projectRoot: string): AftProjectTransport {
    this.assertUsable();
    return this.transport(this.pool.getBridge(projectRoot));
  }

  getActiveBridgeForRoot(projectRoot: string): AftProjectTransport | null {
    if (this.released && !this.currentHold()) return null;
    this.assertUsable();
    const bridge = this.pool.getActiveBridgeForRoot(projectRoot);
    return bridge ? this.transport(bridge) : null;
  }

  activeBridges(): AftProjectTransport[] {
    if (this.released && !this.currentHold()) return [];
    this.assertUsable();
    return this.pool.activeBridges().map((bridge) => this.transport(bridge));
  }

  toolCall(
    projectRoot: string,
    runtime: BridgeToolCallRuntime,
    name: string,
    rawArgs?: ToolCallArguments,
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    return this.dispatch(() => this.pool.toolCall(projectRoot, runtime, name, rawArgs, options));
  }

  setConfigureOverride(key: string, value: unknown): void {
    this.assertUsable();
    this.pool.setConfigureOverride(key, value);
  }

  reconfigure(projectRoot: string, overrides: Record<string, unknown>): Promise<void> {
    return this.dispatch(() => this.pool.reconfigure(projectRoot, overrides));
  }

  replaceBinary(path: string): Promise<string> {
    return this.dispatch(() => this.pool.replaceBinary(path));
  }

  closeSession(projectRoot: string, session: string): Promise<void> {
    if (this.released && !this.currentHold()) return Promise.resolve();
    return this.dispatch(() => this.pool.closeSession(projectRoot, session));
  }

  isShutdown(): boolean {
    return (this.released && !this.currentHold()) || this.pool.isShutdown();
  }

  shutdown(): Promise<void> {
    return this[RELEASE_LOCATION]();
  }

  healthBridges(): AftProjectTransport[] {
    return this.released ? [] : this.pool.activeBridges();
  }

  [RELEASE_LOCATION](): Promise<void> {
    if (this.releasePromise) return this.releasePromise;
    this.released = true;
    this.releasePromise = this.drain().then(() => this.releaseOwner());
    return this.releasePromise;
  }

  private async drain(): Promise<void> {
    if (this.holds.size === 0) return;
    const clock = this.dependencies.executionClock ?? EXECUTION_CLOCK;
    let timer: ReturnType<ExecutionClock["setTimeout"]> | undefined;
    try {
      await new Promise<void>((resolve) => {
        this.drained = resolve;
        // The deadline only releases this Location's reference. Calls can keep
        // using a shared owner; only actual owner shutdown interrupts them.
        timer = clock.setTimeout(
          resolve,
          this.dependencies.executionDrainTimeoutMs ?? EXECUTION_DRAIN_TIMEOUT_MS,
        );
      });
    } finally {
      if (timer !== undefined) clock.clearTimeout(timer);
      this.drained = undefined;
    }
  }
}

/** Admit one complete tool invocation on its original Location, before any await. */
export function withLocationExecution<T>(
  pool: AftTransportPool,
  operation: () => Promise<T>,
): Promise<T> {
  const execute = (pool as Partial<ReleasableLocationPool>)[EXECUTE_LOCATION];
  return execute ? (execute.call(pool, operation) as Promise<T>) : operation();
}

/**
 * Acquire the transport resources for one host Location. Standalone Locations
 * share one process-global owner; subc Locations each own one client while the
 * daemon remains externally supervised. Acquisition and disposal for identical
 * canonical directories pass through the same process-global lock.
 */
export async function acquireBridge(
  locationDirectory: string,
  options: AftTransportFactoryOptions,
  dependencies: AcquireBridgeDependencies = {},
): Promise<AftTransportPool> {
  const directory = canonicalizeProjectRoot(locationDirectory);
  const createPool = dependencies.createPool ?? createAftTransportPool;
  const mode = modeFor(options);

  return withLock(directory, async () => {
    if (mode === "subc") {
      const pool = await createPool(options);
      const ownerExecutions = new Set<() => void>();
      let lease!: LocationBridgeLease;
      lease = new LocationBridgeLease(
        directory,
        mode,
        pool,
        async () => {
          await withLock(directory, async () => {
            lifecycleState().subcLeases.delete(lease);
            for (const interrupt of ownerExecutions) interrupt();
            await pool.shutdown();
          });
        },
        ownerExecutions,
        dependencies,
      );
      lifecycleState().subcLeases.add(lease);
      return lease;
    }

    return withLock(OWNER_LOCK, async () => {
      const state = lifecycleState();
      let owner = state.standalone;
      if (!owner) {
        const configByRoot = new Map<string, Record<string, unknown>>();
        const versionMismatchHandlers = new Map<LocationBridgeLease, VersionMismatchCallback>();
        const configuredLoader = options.poolOptions.projectConfigLoader;
        const pool = await createPool({
          ...options,
          configOverrides: {},
          poolOptions: {
            ...options.poolOptions,
            projectConfigLoader: (projectRoot) => ({
              ...(configuredLoader?.(projectRoot) ?? {}),
              ...(configByRoot.get(canonicalizeProjectRoot(projectRoot)) ?? {}),
            }),
            // The most recently acquired live Location answers. With none live
            // the last lease is being released and the pool is shutting down,
            // so null declines the upgrade and the running binary stays.
            onVersionMismatch: (binaryVersion, minVersion) =>
              [...versionMismatchHandlers.values()].at(-1)?.(binaryVersion, minVersion) ?? null,
          },
        });
        owner = {
          pool,
          configByRoot,
          versionMismatchHandlers,
          referencesByRoot: new Map(),
          references: 0,
        };
        state.standalone = owner;
      }

      owner.references += 1;
      owner.referencesByRoot.set(directory, (owner.referencesByRoot.get(directory) ?? 0) + 1);
      owner.configByRoot.set(directory, { ...options.configOverrides });
      const ownerExecutions = (owner.executionInterrupts ??= new Set<() => void>());
      const acquiredOwner = owner;
      const lease: LocationBridgeLease = new LocationBridgeLease(
        directory,
        mode,
        owner.pool,
        async () => {
          await withLock(directory, () =>
            withLock(OWNER_LOCK, async () => {
              const current = lifecycleState().standalone;
              if (current !== acquiredOwner) return;
              current.versionMismatchHandlers?.delete(lease);
              current.references -= 1;
              const rootReferences = (current.referencesByRoot.get(directory) ?? 1) - 1;
              if (rootReferences > 0) current.referencesByRoot.set(directory, rootReferences);
              else {
                current.referencesByRoot.delete(directory);
                current.configByRoot.delete(directory);
              }
              if (current.references > 0) return;
              lifecycleState().standalone = null;
              for (const interrupt of ownerExecutions) interrupt();
              await current.pool.shutdown();
            }),
          );
        },
        ownerExecutions,
        dependencies,
      );
      const onVersionMismatch = options.poolOptions.onVersionMismatch;
      if (onVersionMismatch) owner.versionMismatchHandlers?.set(lease, onVersionMismatch);
      return lease;
    });
  });
}

/** Release one Location lease. Non-lifecycle pools retain shutdown compatibility. */
export async function releaseBridge(pool: AftTransportPool): Promise<void> {
  const releasable = pool as Partial<ReleasableLocationPool>;
  const release = releasable[RELEASE_LOCATION];
  if (typeof release === "function") {
    await release.call(pool);
    return;
  }
  await pool.shutdown();
}

function numericField(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : 0;
}

function maxField(
  values: readonly Record<string, unknown>[],
  read: (value: Record<string, unknown>) => unknown,
): number {
  return values.reduce((maximum, value) => Math.max(maximum, numericField(read(value))), 0);
}

/**
 * Sample daemon-reported lifecycle health without creating a bridge. Once every
 * Location has released, the empty process topology is returned after the same
 * settle window used by host leak probes.
 */
export async function sampleBridgeLifecycleCensus(
  options: BridgeLifecycleCensusOptions = {},
): Promise<BridgeLifecycleCensus> {
  const settleMs = options.settleMs ?? 2_000;
  if (settleMs > 0) await new Promise((resolve) => setTimeout(resolve, settleMs));

  const state = lifecycleState();
  const bridges = new Set<AftProjectTransport>(state.standalone?.pool.activeBridges() ?? []);
  for (const lease of state.subcLeases) {
    for (const bridge of lease.healthBridges()) bridges.add(bridge);
  }
  const responses = await Promise.allSettled(
    [...bridges].map((bridge) =>
      bridge.send("status", {}, { abortSignal: options.abortSignal, keepBridgeOnTimeout: true }),
    ),
  );
  const status = responses.flatMap((result) =>
    result.status === "fulfilled" ? [result.value] : [],
  );
  if (bridges.size > 0 && status.length === 0) {
    throw new Error("AFT lifecycle census could not read any live daemon health surface");
  }

  return {
    watchers: maxField(
      status,
      (value) => (value.runtime as Record<string, unknown>)?.live_watchers,
    ),
    listenPorts: maxField(
      status,
      (value) => (value.runtime as Record<string, unknown>)?.listen_ports,
    ),
    routes: maxField(status, (value) => (value.runtime as Record<string, unknown>)?.open_routes),
    lspChildren: maxField(
      status,
      (value) => (value.lsp as Record<string, unknown>)?.children_total ?? value.lsp_servers,
    ),
    daemonProcesses: getBridgeLifecycleTopology().daemonProcesses,
  };
}

/** Process-local topology used by lifecycle probes; it never starts a daemon. */
export function getBridgeLifecycleTopology(): BridgeLifecycleTopology {
  const state = lifecycleState();
  const standaloneLocations = state.standalone?.references ?? 0;
  const subcLocations = state.subcLeases.size;
  const daemonProcesses = state.standalone
    ? Math.min(1, state.standalone.pool.activeBridges().length)
    : 0;
  return {
    daemonProcesses,
    routes: standaloneLocations + subcLocations,
    subcClients: subcLocations,
    locations: standaloneLocations + subcLocations,
  };
}
