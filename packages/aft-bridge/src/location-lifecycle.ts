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
const OWNER_LOCK = "process-global-owner";

type TransportMode = "standalone" | "subc";
type PoolFactory = (options: AftTransportFactoryOptions) => Promise<AftTransportPool>;

export interface AcquireBridgeDependencies {
  /** Construction seam for lifecycle tests. Production always uses createAftTransportPool. */
  createPool?: PoolFactory;
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
  if (released) throw new Error("AFT bridge Location lease has already been released");
}

/**
 * A Location receives its own idempotent lease even when it shares the standalone
 * owner. This prevents one finalizer from shutting down resources still used by a
 * sibling Location and makes duplicate host disposal harmless.
 */
class LocationBridgeLease implements ReleasableLocationPool {
  private released = false;
  private releasePromise: Promise<void> | null = null;

  constructor(
    readonly directory: string,
    readonly mode: TransportMode,
    private readonly pool: AftTransportPool,
    private readonly releaseOwner: () => Promise<void>,
  ) {}

  getBridge(projectRoot: string): AftProjectTransport {
    assertLive(this.released);
    return this.pool.getBridge(projectRoot);
  }

  getActiveBridgeForRoot(projectRoot: string): AftProjectTransport | null {
    if (this.released) return null;
    return this.pool.getActiveBridgeForRoot(projectRoot);
  }

  activeBridges(): AftProjectTransport[] {
    if (this.released) return [];
    return this.pool.activeBridges();
  }

  toolCall(
    projectRoot: string,
    runtime: BridgeToolCallRuntime,
    name: string,
    rawArgs?: ToolCallArguments,
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    assertLive(this.released);
    return this.pool.toolCall(projectRoot, runtime, name, rawArgs, options);
  }

  setConfigureOverride(key: string, value: unknown): void {
    assertLive(this.released);
    this.pool.setConfigureOverride(key, value);
  }

  reconfigure(projectRoot: string, overrides: Record<string, unknown>): Promise<void> {
    assertLive(this.released);
    return this.pool.reconfigure(projectRoot, overrides);
  }

  replaceBinary(path: string): Promise<string> {
    assertLive(this.released);
    return this.pool.replaceBinary(path);
  }

  closeSession(projectRoot: string, session: string): Promise<void> {
    if (this.released) return Promise.resolve();
    return this.pool.closeSession(projectRoot, session);
  }

  isShutdown(): boolean {
    return this.released || this.pool.isShutdown();
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
    this.releasePromise = this.releaseOwner();
    return this.releasePromise;
  }
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
      let lease!: LocationBridgeLease;
      lease = new LocationBridgeLease(directory, mode, pool, async () => {
        await withLock(directory, async () => {
          lifecycleState().subcLeases.delete(lease);
          await pool.shutdown();
        });
      });
      lifecycleState().subcLeases.add(lease);
      return lease;
    }

    return withLock(OWNER_LOCK, async () => {
      const state = lifecycleState();
      let owner = state.standalone;
      if (!owner) {
        const configByRoot = new Map<string, Record<string, unknown>>();
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
          },
        });
        owner = { pool, configByRoot, referencesByRoot: new Map(), references: 0 };
        state.standalone = owner;
      }

      owner.references += 1;
      owner.referencesByRoot.set(directory, (owner.referencesByRoot.get(directory) ?? 0) + 1);
      owner.configByRoot.set(directory, { ...options.configOverrides });
      const acquiredOwner = owner;
      return new LocationBridgeLease(directory, mode, owner.pool, async () => {
        await withLock(directory, () =>
          withLock(OWNER_LOCK, async () => {
            const current = lifecycleState().standalone;
            if (current !== acquiredOwner) return;
            current.references -= 1;
            const rootReferences = (current.referencesByRoot.get(directory) ?? 1) - 1;
            if (rootReferences > 0) current.referencesByRoot.set(directory, rootReferences);
            else {
              current.referencesByRoot.delete(directory);
              current.configByRoot.delete(directory);
            }
            if (current.references > 0) return;
            lifecycleState().standalone = null;
            await current.pool.shutdown();
          }),
        );
      });
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
