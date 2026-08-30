/**
 * The SINGLE injection point that selects AFT's transport: standalone NDJSON
 * (spawn the `aft` binary, today's default) or the Subconscious daemon (talk to
 * AFT as a supervised module). Both plugins call this one factory so the choice
 * lives in exactly one place and everything downstream (tool registration,
 * hoisting, permission UI, sidebar) stays transport-agnostic behind the shared
 * {@link AftTransportPool} interface.
 *
 * Selection is by the USER-tier `subc.connection_file` config key (a project
 * config can never set it — enforced in each plugin's config loader). Present +
 * the file exists ⇒ subc; absent/empty ⇒ standalone (the default). Present but
 * the file is MISSING ⇒ FAIL LOUD (throw) — never a silent downgrade to
 * standalone, which would split-brain a user who meant to run under the daemon.
 */

import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { isAbsolute, join } from "node:path";

import type { ConsumerIdentity } from "@cortexkit/subc-client";

import {
  type CanonicalRootPath,
  type ConcretePoolId,
  getLifecycleRegistry,
  type LifecycleRegistry,
  type RootGeneration,
} from "./lifecycle-registry.js";
import { BridgePool, type PoolOptions } from "./pool.js";
import { RevivableTransportPool } from "./revivable-transport.js";
import { type BgNudgeRef, SubcTransportPool } from "./subc-transport.js";
import type { AftTransportPool } from "./transport.js";

export interface AftTransportFactoryOptions {
  /** Harness identity ("opencode" | "pi"). Carried in every subc BindIdentity. */
  harness: string;
  /** Standalone path: resolved `aft` binary. */
  binaryPath: string;
  /** Standalone path: pool/bridge options (callbacks, timeouts, project loader). */
  poolOptions: PoolOptions;
  /** Standalone path: global configure overrides baked into every bridge. */
  configOverrides: Record<string, unknown>;
  /**
   * USER-tier `subc.connection_file` (already stripped of any project override).
   * Present + existing ⇒ subc transport; absent/empty ⇒ standalone. Tilde and
   * relative paths are resolved against the user's home directory.
   */
  subcConnectionFile?: string;
  /** Test/in-process override for route principal identity. Production leaves this undefined. */
  subcConsumerIdentity?: ConsumerIdentity | null;
  /**
   * User-tier `subc.client_reaper`. The value is captured in each concrete
   * registration and is never mutated in place. Omitted means the merge default
   * is off; a production candidate may opt in by passing true explicitly.
   */
  subcClientReaper?: boolean;
  /** Optional registry override for tests; production uses this module's shared lifecycle registry. */
  lifecycleRegistry?: LifecycleRegistry;
  /**
   * Synchronous filesystem-demand seam used by lifecycle-enabled getBridge calls.
   * Production defaults to checking that the canonical root exists.
   */
  subcLifecycleDemandCheck?: (
    root: CanonicalRootPath,
    poolId: ConcretePoolId,
  ) => boolean | { readonly exists?: boolean };
  /**
   * Subc path: idle bg-completion wake handler (a `{op:"bg_events"}` nudge). The
   * handler MUST force a drain (the nudge is payload-less). Ignored standalone.
   */
  onBgEventsNudge?: (projectRoot: string, session: string) => void;
  /** Subc path: idle bg-completion wake handler with complete root provenance. */
  onBgEventsNudgeRef?: (ref: BgNudgeRef) => void;
}

function resolveConnectionFilePath(raw: string): string {
  const trimmed = raw.trim();
  if (trimmed.startsWith("~")) {
    return join(homedir(), trimmed.slice(1).replace(/^[/\\]/, ""));
  }
  if (isAbsolute(trimmed)) return trimmed;
  // A bare/relative path is resolved against home, not the project cwd — this is
  // a per-machine daemon endpoint, never a project-relative artifact.
  return join(homedir(), trimmed);
}

const SUBC_CLIENT_REAPER_PROCESS_KEY = "subc_client_reaper";

function booleanOption(value: unknown): boolean | undefined {
  return typeof value === "boolean" ? value : undefined;
}

/**
 * The wrapper owns the outer facade map. The map is intentionally private to
 * RevivableTransportPool; this narrow adapter keeps the lifecycle callback
 * synchronous without exposing that implementation detail as a public API.
 */
function evictRevivableFacade(
  wrapper: RevivableTransportPool,
  root: CanonicalRootPath,
  _generation: RootGeneration,
): void {
  // A registry callback is generation-matched before it reaches this function.
  // Check the non-creating active lookup as an additional guard so a successor
  // already exposed by a different concrete pool is never removed.
  if (wrapper.currentBridge(root) !== null) return;
  const internals = wrapper as unknown as { transports: Map<string, unknown> };
  internals.transports.delete(root);
}

function satisfyLifecycleDemand(wrapper: RevivableTransportPool, projectRoot: string): void {
  const activePool = (wrapper as unknown as { activePool: AftTransportPool }).activePool;
  if (activePool.isShutdown() || !(activePool instanceof SubcTransportPool)) return;
  // The outer wrapper's synchronous getBridge is the explicit demand boundary.
  // Register the concrete root before the wrapper creates and returns its facade.
  activePool.getBridge(projectRoot);
}

/**
 * Construct the transport pool for this plugin process. Async because the subc
 * presence check stats the connection file. Returns a pool satisfying
 * {@link AftTransportPool} either way; the caller's downstream code does not
 * branch on which. The returned ownership layer replaces a terminal subc or
 * standalone instance when a later tool call arrives after host teardown.
 */
export async function createAftTransportPool(
  opts: AftTransportFactoryOptions,
): Promise<AftTransportPool> {
  let binaryPath = opts.binaryPath;
  const configuredReaper = booleanOption(opts.configOverrides[SUBC_CLIENT_REAPER_PROCESS_KEY]);
  const configuredCamelCaseReaper = booleanOption(opts.configOverrides.subcClientReaper);
  const reapingEnabled =
    opts.subcClientReaper ?? configuredReaper ?? configuredCamelCaseReaper ?? false;
  const configOverrides = { ...opts.configOverrides };
  delete configOverrides[SUBC_CLIENT_REAPER_PROCESS_KEY];
  delete configOverrides.subcClientReaper;
  const concreteOptions = { ...opts, configOverrides };
  const lifecycleRegistry = opts.lifecycleRegistry ?? getLifecycleRegistry();
  let wrapper: RevivableTransportPool | null = null;

  const registerConcretePool = (pool: AftTransportPool): void => {
    if (!(pool instanceof SubcTransportPool)) return;
    const owner = wrapper;
    if (!owner) throw new Error("subc lifecycle registration requires its wrapper");
    pool.registerLifecyclePool(lifecycleRegistry, {
      reapingEnabled,
      evictOuterFacade: (root, generation) => evictRevivableFacade(owner, root, generation),
    });
  };

  // Construct the initial concrete pool without lifecycle participation. Create
  // the wrapper before binding its callback, then register the concrete pool only
  // after the wrapper exists so lifecycle cleanup can safely inspect it.
  const initialPool = await createConcreteAftTransportPool({ ...concreteOptions, binaryPath });
  const createRegisteredPool = async (): Promise<AftTransportPool> => {
    const replacement = await createConcreteAftTransportPool({ ...concreteOptions, binaryPath });
    // ensureActivePool publishes a replacement only after this promise settles.
    registerConcretePool(replacement);
    return replacement;
  };
  wrapper = new RevivableTransportPool(initialPool, createRegisteredPool, (path) => {
    binaryPath = path;
  });
  if (reapingEnabled) {
    const owner = wrapper;
    const getBridge = owner.getBridge.bind(owner);
    owner.getBridge = (projectRoot) => {
      satisfyLifecycleDemand(owner, projectRoot);
      return getBridge(projectRoot);
    };
  }
  registerConcretePool(initialPool);
  return wrapper;
}

async function createConcreteAftTransportPool(
  opts: AftTransportFactoryOptions,
): Promise<AftTransportPool> {
  const raw = opts.subcConnectionFile?.trim();
  if (raw && raw.length > 0) {
    const connectionFile = resolveConnectionFilePath(raw);
    const available = await SubcTransportPool.connectionAvailable(connectionFile);
    if (!available) {
      // FAIL LOUD: the user explicitly selected subc but the daemon's connection
      // file is absent. Downgrading to standalone here would split-brain a user
      // who expects the daemon to own indexes/caches — surface the error so they
      // start the daemon or clear the config.
      throw new Error(
        `subc.connection_file is set to "${raw}" (resolved: ${connectionFile}) but no subc ` +
          `connection file exists there. Start the Subconscious daemon, correct the path, ` +
          `or remove subc.connection_file from your user config to use the standalone bridge.`,
      );
    }
    return new SubcTransportPool({
      connectionFile,
      harness: opts.harness,
      consumerIdentity: opts.subcConsumerIdentity,
      onBgEventsNudge: opts.onBgEventsNudge,
      onBgEventsNudgeRef: opts.onBgEventsNudgeRef,
      lifecycleDemandCheck: opts.subcLifecycleDemandCheck ?? ((root) => existsSync(root)),
      defaultTimeoutMs: opts.poolOptions.timeoutMs ?? 30_000,
    });
  }
  return new BridgePool(opts.binaryPath, opts.poolOptions, opts.configOverrides);
}
