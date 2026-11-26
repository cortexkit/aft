import { homedir } from "node:os";

import { warn as activeWarn, error, getActiveLogger, log } from "./active-logger.js";
import { BinaryBridge, type BridgeOptions } from "./bridge.js";
import { asCanonicalRootPath, type CanonicalRootPath } from "./lifecycle-registry.js";
import type { Logger, LogMeta } from "./logger.js";
import { canonicalizeProjectRoot } from "./project-identity.js";
import type {
  AftTransportPool,
  ToolCallArguments,
  ToolCallOptions,
  ToolCallResult,
} from "./transport.js";

const DEFAULT_IDLE_TIMEOUT_MS = 30 * 60 * 1000; // evict idle bridges after 30 minutes
const DEFAULT_MAX_POOL_SIZE = 8;
const CLEANUP_INTERVAL_MS = 60 * 1000; // check every minute

/**
 * Historical error class — kept for backwards-compatible imports.
 *
 * **No longer thrown by `BridgePool.getBridge()`.** Prior versions refused
 * to spawn a bridge when `project_root` resolved to `$HOME`, but that was
 * too restrictive: legitimate migration tasks (e.g. shell config sweeps,
 * dotfile maintenance) need to operate from `$HOME` directly. The Rust
 * `handle_configure` now auto-disables heavy subsystems
 * (`search_index`, `semantic_search`) and records `degraded_reasons:
 * ["home_root"]` on the status snapshot, so the bridge spawns fast,
 * `read`/`write`/`edit`/`bash` work, and the sidebar / `/aft-status`
 * surfaces the degraded state. See `crates/aft/src/commands/configure.rs`
 * for the full reasoning.
 *
 * Plugins still skip *eager* configure on `$HOME` (Desktop launches from
 * `~` shouldn't auto-warm a bridge no one asked for), but lazy configure
 * on the first real tool call works in degraded mode.
 */
export class HomeProjectRootError extends Error {
  constructor(public readonly projectRoot: string) {
    super(
      `aft refuses to spawn a bridge with project_root=${projectRoot} (user home directory). ` +
        `Open OpenCode/Pi from a project subdirectory instead, or set the session's ` +
        `directory to a real project root.`,
    );
    this.name = "HomeProjectRootError";
  }
}

/**
 * Canonicalize the user's home directory for stable comparison with bridge
 * keys. Uses the SAME canonicalizer as `normalizeKey` so a `$HOME` spelled with
 * a symlink/trailing-slash/Windows-drive-case still matches a bridge key.
 */
function canonicalHomeDir(): string | null {
  try {
    const home = homedir();
    if (!home) return null;
    return canonicalizeProjectRoot(home);
  } catch {
    return null;
  }
}

/**
 * Test whether the given normalized project root matches the user's home
 * directory exactly. Subdirectories of `$HOME` are valid project roots and
 * pass through.
 */
export function isHomeDirectoryRoot(normalizedKey: string): boolean {
  const home = canonicalHomeDir();
  if (!home) return false;
  return normalizedKey === home;
}

interface PoolEntry {
  bridge: BinaryBridge;
  lastUsed: number;
}

export interface BridgeToolCallRuntime {
  sessionID?: string;
}

export interface PoolOptions extends BridgeOptions {
  /** Soft cap: idle bridges are evicted to keep this size; busy bridges are never cut. */
  maxPoolSize?: number;
  idleTimeoutMs?: number;
  logger?: Logger;
  /**
   * Optional per-project configure override loader. Called exactly once when
   * a new bridge is spawned for `projectRoot`, with the canonical (already
   * normalized) project root. Returned overrides are deep-merged on top of
   * the pool's global `configOverrides` and shallow-merged into the bridge's
   * configure payload (per-project values win).
   *
   * Use this when one plugin instance serves many projects (OpenCode Desktop /
   * `opencode serve`) and each project has its own `.opencode/aft.jsonc` whose
   * fields differ from the user-level config. Without this loader, only the
   * project config visible at plugin init reaches the Rust side; later
   * sessions opened in other projects inherit the wrong project's overrides.
   *
   * Caveats:
   *   - The loader runs synchronously inside `getBridge()`. Keep it cheap.
   *   - Existing bridges keep the overrides they were spawned with — this is
   *     intentional so reloads don't blow away warm trigram/LSP/semantic state.
   *   - The loader should ONLY return per-project-overridable fields. Truly
   *     global fields (storage_dir, _ort_dylib_dir, harness, lsp_paths_extra)
   *     belong in the pool's static `configOverrides` constructor argument.
   *
   * If the loader throws, the bridge falls back to global overrides only and
   * the error is logged via the pool logger.
   */
  projectConfigLoader?: (projectRoot: string) => Record<string, unknown>;
}

/**
 * Manages a pool of BinaryBridge instances, keyed by **canonical project root**.
 *
 * Prior to issue #14, the pool spawned one binary process per OpenCode session,
 * which duplicated every heavy in-memory structure (ONNX runtime, trigram and
 * semantic indexes, LSP state, symbol caches) N times for N sessions in the
 * same project. That produced an effective "leak" the user saw as many aft
 * processes consuming gigabytes of RAM on large repositories.
 *
 * The current design spawns **one bridge per project** and relies on the Rust
 * side to partition the small amount of truly session-scoped state (undo
 * history, named checkpoints) via the `session_id` envelope field attached by
 * the `callBridge()` helper. Sessions sharing a bridge still share the
 * latency of a single request pipeline; the trade-off is acceptable because
 * it removes the real RAM multiplier. `maxPoolSize` is a soft cap: idle
 * bridges are evicted to restore it, while busy bridges remain available.
 */
export class BridgePool implements AftTransportPool {
  private readonly evictionListeners = new Set<(root: string) => void>();
  /** Project-root → bridge. Key is a normalized canonical path. */
  private readonly bridges = new Map<string, PoolEntry>();
  private readonly staleBridges = new Set<BinaryBridge>();
  private binaryPath: string;
  private readonly maxPoolSize: number;
  private readonly idleTimeoutMs: number;
  private readonly bridgeOptions: BridgeOptions;
  private readonly configOverrides: Record<string, unknown>;
  private editSlotSurvives: boolean | undefined;
  private editSlotSurvivesCaptured = false;
  private readonly projectConfigLoader:
    | ((projectRoot: string) => Record<string, unknown>)
    | undefined;
  private readonly logger: Logger | undefined;
  private cleanupTimer: ReturnType<typeof setInterval> | null = null;
  private shutdownCalled = false;

  constructor(
    binaryPath: string,
    options: PoolOptions = {},
    configOverrides: Record<string, unknown> = {},
  ) {
    this.binaryPath = binaryPath;
    this.maxPoolSize = options.maxPoolSize ?? DEFAULT_MAX_POOL_SIZE;
    this.idleTimeoutMs = options.idleTimeoutMs ?? DEFAULT_IDLE_TIMEOUT_MS;
    this.logger = options.logger;
    this.projectConfigLoader = options.projectConfigLoader;
    this.bridgeOptions = {
      timeoutMs: options.timeoutMs,
      maxRestarts: options.maxRestarts,
      minVersion: options.minVersion,
      onVersionMismatch: options.onVersionMismatch,
      onConfigureWarnings: options.onConfigureWarnings,
      onBashCompletion: options.onBashCompletion,
      onBashLongRunning: options.onBashLongRunning,
      onBashPatternMatch: options.onBashPatternMatch,
      errorPrefix: options.errorPrefix,
      logger: options.logger,
      // Forward the per-child env override so a pooled bridge honors the
      // documented `BridgeOptions.childEnv` (PoolOptions extends BridgeOptions);
      // omitting it silently dropped the override for pooled spawns.
      childEnv: options.childEnv,
      // The hang watchdog's window floor, probe timeout and the time source (tests inject a
      // manual clock); without these a pooled bridge silently used the
      // defaults whatever the pool was given.
      hangWindowMinMs: options.hangWindowMinMs,
      hangProbeTimeoutMs: options.hangProbeTimeoutMs,
      clock: options.clock,
    };
    this.configOverrides = { ...configOverrides };
    const initialEditSlotSurvives = this.configOverrides.edit_slot_survives;
    delete this.configOverrides.edit_slot_survives;
    if (initialEditSlotSurvives !== undefined) {
      if (typeof initialEditSlotSurvives !== "boolean") {
        throw new Error("edit_slot_survives must be a boolean");
      }
      this.editSlotSurvives = initialEditSlotSurvives;
      this.editSlotSurvivesCaptured = true;
    }
    // Reuse the existing idle-window timer for both idle eviction and binary
    // refresh checks. When idle eviction is disabled, there is no background
    // maintenance loop.
    this.startCleanupTimer();
  }

  /**
   * Get an alive bridge only when it belongs to the requested project root.
   *
   * Used by read-only paths (e.g. `/aft-status`, background-bash drains) that
   * want to reuse a warm bridge with loaded indexes/LSP state. Returns `null`
   * when no live bridge exists for `projectRoot`; callers typically fall back
   * to {@link BridgePool.getBridge} which will create one. Cross-project bridge
   * sharing is intentionally **not** supported — draining bg-completions or
   * status from another project's bridge mixes session-isolated state.
   */
  getActiveBridgeForRoot(projectRoot: string): BinaryBridge | null {
    if (this.shutdownCalled) return null;
    const key = normalizeKey(projectRoot);
    const entry = this.bridges.get(key);
    if (!entry?.bridge.isAlive()) return null;
    entry.lastUsed = Date.now();
    return entry.bridge;
  }

  /** All live bridges, for session-scoped signals that must not depend on
   * exact root-key resolution (the command itself carries the session ID). */
  activeBridges(): BinaryBridge[] {
    if (this.shutdownCalled) return [];
    const alive: BinaryBridge[] = [];
    for (const entry of this.bridges.values()) {
      if (entry.bridge.isAlive()) alive.push(entry.bridge);
    }
    return alive;
  }

  /**
   * Get or create the bridge for `projectRoot`.
   *
   * Callers should always pass a **canonical** project root (see
   * `projectRootFor()` in `tools/_shared.ts`). All sessions operating on the
   * same project share one bridge; their undo/checkpoint state is still
   * isolated by `session_id` on the Rust side.
   */
  getBridge(projectRoot: string): BinaryBridge {
    return this.getBridgeForCanonicalRoot(asCanonicalRootPath(normalizeKey(projectRoot)));
  }

  getBridgeForCanonicalRoot(key: CanonicalRootPath): BinaryBridge {
    if (this.shutdownCalled) {
      this.shutdownCalled = false;
      this.startCleanupTimer();
    }

    // `$HOME`-rooted spawns are no longer refused here. The Rust
    // `handle_configure` auto-disables heavy subsystems (search_index,
    // semantic_search) for `$HOME` and records the state on the status
    // snapshot. Plugins still skip *eager* configure on `$HOME` (Desktop
    // launches from `~` shouldn't auto-warm a bridge no one asked for),
    // but lazy configure on the first real tool call now works in
    // degraded mode — see HomeProjectRootError doc-comment above.

    const existing = this.bridges.get(key);
    if (existing) {
      existing.lastUsed = Date.now();
      return existing.bridge;
    }

    // Evict the least-recently-used idle bridge at the soft cap. Busy bridges
    // stay alive so opening another project cannot cancel their work.
    if (this.bridges.size >= this.maxPoolSize) {
      this.evictLRU();
      if (this.bridges.size >= this.maxPoolSize) {
        this.warn(
          `bridge pool exceeded soft maxPoolSize=${this.maxPoolSize}; all existing bridges have pending requests or background tasks`,
        );
      }
    }

    // Per-project overrides ON TOP of global overrides (loader values win).
    // Without this, OpenCode Desktop / `opencode serve` would inherit whatever
    // project's `.opencode/aft.jsonc` was visible at plugin init for ALL
    // sessions, ignoring the actual session's project config. See the
    // `projectConfigLoader` doc-comment on PoolOptions.
    const projectOverrides = this.loadProjectOverrides(key);
    const mergedOverrides = { ...this.configOverrides, ...projectOverrides };
    delete mergedOverrides.edit_slot_survives;

    const bridge = new BinaryBridge(
      this.binaryPath,
      key,
      this.bridgeOptions,
      mergedOverrides,
      this.editSlotSurvivesCaptured ? this.editSlotSurvives : undefined,
    );
    this.bridges.set(key, { bridge, lastUsed: Date.now() });
    return bridge;
  }

  async toolCall(
    projectRoot: string,
    runtime: BridgeToolCallRuntime,
    name: string,
    rawArgs: ToolCallArguments = {},
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    return this.getBridge(projectRoot).toolCall(runtime.sessionID, name, rawArgs, options);
  }

  subscribeBridgeEvictions(listener: (root: string) => void): () => void {
    this.evictionListeners.add(listener);
    return () => {
      this.evictionListeners.delete(listener);
    };
  }

  private removeBridge(root: string): void {
    if (!this.bridges.delete(root)) return;
    for (const listener of this.evictionListeners) listener(root);
  }

  /** Periodic pool maintenance: retire updated binaries and evict idle bridges. */
  private cleanup(): void {
    const now = Date.now();
    for (const [dir, entry] of this.bridges) {
      if (entry.bridge.hasPendingRequests() || entry.bridge.hasOutstandingBackgroundTasks())
        continue;
      if (entry.bridge.maybeScheduleRespawnForUpdatedBinary(CLEANUP_INTERVAL_MS, now)) {
        this.staleBridges.add(entry.bridge);
        this.removeBridge(dir);
        continue;
      }
      if (now - entry.lastUsed > this.idleTimeoutMs) {
        entry.bridge.shutdown().catch((err) => this.error("cleanup shutdown failed:", err));
        this.removeBridge(dir);
      }
    }

    while (this.bridges.size > this.maxPoolSize) {
      const sizeBefore = this.bridges.size;
      this.evictLRU();
      if (this.bridges.size === sizeBefore) break;
    }

    for (const bridge of this.staleBridges) {
      if (bridge.hasPendingRequests() || bridge.hasOutstandingBackgroundTasks()) continue;
      bridge.shutdown().catch((err) => this.error("stale cleanup shutdown failed:", err));
      this.staleBridges.delete(bridge);
    }
  }

  /** Evict the least recently used bridge to make room. */
  private evictLRU(): void {
    let oldestDir: string | null = null;
    let oldestTime = Infinity;
    for (const [dir, entry] of this.bridges) {
      if (entry.bridge.hasPendingRequests() || entry.bridge.hasOutstandingBackgroundTasks())
        continue;
      if (entry.lastUsed < oldestTime) {
        oldestTime = entry.lastUsed;
        oldestDir = dir;
      }
    }
    if (oldestDir) {
      const entry = this.bridges.get(oldestDir);
      entry?.bridge.shutdown().catch((err) => this.error("eviction shutdown failed:", err));
      this.removeBridge(oldestDir);
    }
  }

  /**
   * No-op for the standalone transport: bridges are per-project (shared across a
   * project's sessions), and per-session state (undo/checkpoint/bash) lives
   * Rust-side keyed by session_id — there is nothing session-scoped to tear down
   * on the plugin side. Present to satisfy {@link AftTransportPool} so the subc
   * pool's per-session route teardown is callable transport-agnostically.
   */
  async closeSession(_projectRoot: string, _session: string): Promise<void> {}

  /** Shut down all bridges and stop the cleanup timer. */
  async shutdown(): Promise<void> {
    this.shutdownCalled = true;
    if (this.cleanupTimer) {
      clearInterval(this.cleanupTimer);
      this.cleanupTimer = null;
    }
    const shutdowns = [
      ...Array.from(this.bridges.values(), (e) => e.bridge.shutdown()),
      ...Array.from(this.staleBridges.values(), (bridge) => bridge.shutdown()),
    ];
    for (const root of Array.from(this.bridges.keys())) this.removeBridge(root);
    this.staleBridges.clear();
    await Promise.allSettled(shutdowns);
  }

  /** A standalone pool can be restarted by its owner after shutdown. */
  isShutdown(): boolean {
    return this.shutdownCalled;
  }

  /**
   * Replace the binary path and restart all bridges.
   * Used after downloading a newer binary version.
   */
  async replaceBinary(newPath: string): Promise<string> {
    this.binaryPath = newPath;
    // Move current pool entries aside so next getBridge() creates fresh bridges
    // with the new binary, while still keeping the old processes reachable for
    // cleanup/shutdown. Do NOT call shutdown() here: when replaceBinary() is
    // invoked from a bridge's onVersionMismatch callback, shutdown() marks that
    // in-flight bridge as shutting down before BinaryBridge.replaceCurrentBinary()
    // can restart it, breaking the transparent retry path. Existing bridge
    // processes are left to finish their current calls; cleanup drains them once
    // they no longer have pending work.
    for (const entry of this.bridges.values()) {
      this.staleBridges.add(entry.bridge);
    }
    for (const root of Array.from(this.bridges.keys())) this.removeBridge(root);
    this.log(
      `Binary path updated to ${newPath}. Active bridges marked stale — next calls will use the new binary.`,
    );
    return newPath;
  }

  private startCleanupTimer(): void {
    if (!Number.isFinite(this.idleTimeoutMs) || this.cleanupTimer) return;
    this.cleanupTimer = setInterval(() => this.cleanup(), CLEANUP_INTERVAL_MS);
    this.cleanupTimer.unref(); // don't prevent Node from exiting
  }

  private log(message: string, meta?: LogMeta): void {
    const logger = this.logger ?? getActiveLogger();
    if (logger) {
      try {
        logger.log(message, meta);
      } catch (err) {
        console.error(
          `[aft-bridge] ERROR: pool logger log threw: ${err instanceof Error ? err.message : String(err)}`,
        );
        console.error(`[aft-bridge] ${message}`);
      }
    } else log(message, meta);
  }

  private warn(message: string, meta?: LogMeta): void {
    const logger = this.logger ?? getActiveLogger();
    if (logger) {
      try {
        logger.warn(message, meta);
      } catch (err) {
        console.error(
          `[aft-bridge] ERROR: pool logger warn threw: ${err instanceof Error ? err.message : String(err)}`,
        );
        console.error(`[aft-bridge] WARN: ${message}`);
      }
    } else activeWarn(message, meta);
  }

  private error(message: string, meta?: LogMeta): void {
    const logger = this.logger ?? getActiveLogger();
    if (logger) {
      try {
        logger.error(message, meta);
      } catch (err) {
        console.error(
          `[aft-bridge] ERROR: pool logger error threw: ${err instanceof Error ? err.message : String(err)}`,
        );
        console.error(`[aft-bridge] ERROR: ${message}`);
      }
    } else error(message, meta);
  }

  /**
   * Update a runtime configure override for future bridge spawns. Existing bridges
   * keep those mutable runtime values so their warm state is not discarded.
   *
   * `edit_slot_survives` is host registration state instead: its first boolean
   * value is captured separately and forwarded to every existing and future bridge.
   * Any later write is a lifecycle error rather than a mutable config update.
   */
  setConfigureOverride(key: string, value: unknown): void {
    if (key === "edit_slot_survives") {
      if (typeof value !== "boolean") {
        throw new Error("edit_slot_survives must be set once to a boolean");
      }
      if (this.editSlotSurvivesCaptured) {
        throw new Error("edit_slot_survives is write-once and was already captured");
      }
      this.editSlotSurvives = value;
      this.editSlotSurvivesCaptured = true;
      for (const entry of this.bridges.values()) {
        entry.bridge.setEditSlotSurvives(value);
      }
      for (const bridge of this.staleBridges) {
        bridge.setEditSlotSurvives(value);
      }
      return;
    }

    if (value === undefined) {
      delete this.configOverrides[key];
    } else {
      this.configOverrides[key] = value;
    }
  }

  /**
   * Reapply configure overrides to a live bridge without replacing it.
   *
   * The root selects the immediate live refresh. Overrides also persist
   * pool-wide for future bridges, including bridges for other roots.
   *
   * Async resources such as the LSP install cache can become available after a
   * bridge has configured. Sending a normal configure request keeps the bridge's
   * warm Rust state while allowing those process-state paths to take effect.
   */
  async reconfigure(projectRoot: string, overrides: Record<string, unknown>): Promise<void> {
    const key = normalizeKey(projectRoot);
    for (const [name, value] of Object.entries(overrides)) {
      this.setConfigureOverride(name, value);
    }

    const entry = this.shutdownCalled ? undefined : this.bridges.get(key);
    if (!entry?.bridge.isAlive()) return;
    entry.lastUsed = Date.now();
    const bridge = entry.bridge;

    const projectOverrides = this.loadProjectOverrides(key);
    const response = await bridge.send("configure", {
      project_root: key,
      ...this.configOverrides,
      ...projectOverrides,
      ...overrides,
    });
    if (response.success === false) {
      throw new Error(
        `configure refresh failed for ${key}: ${String(response.message ?? "unknown error")}`,
      );
    }
  }

  private loadProjectOverrides(key: string): Record<string, unknown> {
    if (!this.projectConfigLoader) return {};
    try {
      return this.projectConfigLoader(key) ?? {};
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.error(`projectConfigLoader failed; using global overrides only: ${message}`);
      return {};
    }
  }

  /** Number of active bridges in the pool. */
  get size(): number {
    return this.bridges.size;
  }

  /**
   * Test-only: read the current configure-override map.
   *
   * NEVER call this from production code. The override map is intentionally
   * private because the contract is "applied at next spawn" — exposing the
   * live map invites callers to mutate it directly and bypass the lifecycle.
   * This getter is here so tests for `setConfigureOverride` can verify the
   * mutation result without spawning real binaries.
   */
  _testGetConfigOverrides(): Readonly<Record<string, unknown>> {
    return { ...this.configOverrides };
  }

  /**
   * Test-only view of the per-bridge options forwarded to every spawned
   * `BinaryBridge`. Lets tests assert that documented `BridgeOptions` fields
   * (e.g. `childEnv`) are actually propagated through the pool rather than
   * silently dropped.
   */
  _testGetBridgeOptions(): Readonly<BridgeOptions> {
    return { ...this.bridgeOptions };
  }
}

/**
 * Canonicalize bridge keys so symlinked paths, trailing separators, and Windows
 * verbatim/drive-case spellings collapse to one key. Delegates to the single
 * shared canonicalizer (`canonicalizeProjectRoot`) so bridge routing, RPC
 * port-file scoping, and the sidebar status gate all derive identity the same
 * way — the divergence between them was the sidebar/stale-port bug.
 */
function normalizeKey(projectRoot: string): string {
  return canonicalizeProjectRoot(projectRoot);
}
