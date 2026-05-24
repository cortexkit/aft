import { realpathSync } from "node:fs";
import { homedir } from "node:os";
import { error, getActiveLogger, log } from "./active-logger.js";
import { BinaryBridge, type BridgeOptions } from "./bridge.js";
import type { Logger, LogMeta } from "./logger.js";

const DEFAULT_IDLE_TIMEOUT_MS = Infinity; // keep alive as long as the host is running
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

/** Canonicalize the user's home directory for stable comparison with bridge keys. */
function canonicalHomeDir(): string | null {
  try {
    const home = homedir();
    if (!home) return null;
    try {
      return realpathSync(home);
    } catch {
      return home.replace(/[/\\]+$/, "");
    }
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

export interface PoolOptions extends BridgeOptions {
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
 * it removes the real RAM multiplier.
 */
export class BridgePool {
  /** Project-root → bridge. Key is a normalized canonical path. */
  private readonly bridges = new Map<string, PoolEntry>();
  private binaryPath: string;
  private readonly maxPoolSize: number;
  private readonly idleTimeoutMs: number;
  private readonly bridgeOptions: BridgeOptions;
  private readonly configOverrides: Record<string, unknown>;
  private readonly projectConfigLoader:
    | ((projectRoot: string) => Record<string, unknown>)
    | undefined;
  private readonly logger: Logger | undefined;
  private cleanupTimer: ReturnType<typeof setInterval> | null = null;

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
      errorPrefix: options.errorPrefix,
      logger: options.logger,
    };
    this.configOverrides = configOverrides;
    // Skip cleanup timer when idle timeout is Infinity (no-op) to avoid wasted cycles
    if (Number.isFinite(this.idleTimeoutMs)) {
      this.cleanupTimer = setInterval(() => this.cleanup(), CLEANUP_INTERVAL_MS);
      this.cleanupTimer.unref(); // don't prevent Node from exiting
    }
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
    const key = normalizeKey(projectRoot);
    const entry = this.bridges.get(key);
    if (!entry?.bridge.isAlive()) return null;
    entry.lastUsed = Date.now();
    return entry.bridge;
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
    const key = normalizeKey(projectRoot);

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

    // Evict LRU if at capacity (one project = one slot now, so reaching the
    // cap means the user has many distinct projects open).
    if (this.bridges.size >= this.maxPoolSize) {
      this.evictLRU();
    }

    // Per-project overrides ON TOP of global overrides (loader values win).
    // Without this, OpenCode Desktop / `opencode serve` would inherit whatever
    // project's `.opencode/aft.jsonc` was visible at plugin init for ALL
    // sessions, ignoring the actual session's project config. See the
    // `projectConfigLoader` doc-comment on PoolOptions.
    let projectOverrides: Record<string, unknown> = {};
    if (this.projectConfigLoader) {
      try {
        projectOverrides = this.projectConfigLoader(key) ?? {};
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        this.error(`projectConfigLoader failed; using global overrides only: ${message}`);
      }
    }
    const mergedOverrides = { ...this.configOverrides, ...projectOverrides };

    const bridge = new BinaryBridge(this.binaryPath, key, this.bridgeOptions, mergedOverrides);
    this.bridges.set(key, { bridge, lastUsed: Date.now() });
    return bridge;
  }

  /** Shut down idle bridges that haven't been used within the timeout. */
  private cleanup(): void {
    const now = Date.now();
    for (const [dir, entry] of this.bridges) {
      if (entry.bridge.hasPendingRequests()) continue;
      if (now - entry.lastUsed > this.idleTimeoutMs) {
        entry.bridge.shutdown().catch((err) => this.error("cleanup shutdown failed:", err));
        this.bridges.delete(dir);
      }
    }
  }

  /** Evict the least recently used bridge to make room. */
  private evictLRU(): void {
    let oldestDir: string | null = null;
    let oldestTime = Infinity;
    for (const [dir, entry] of this.bridges) {
      if (entry.bridge.hasPendingRequests()) continue;
      if (entry.lastUsed < oldestTime) {
        oldestTime = entry.lastUsed;
        oldestDir = dir;
      }
    }
    if (oldestDir) {
      const entry = this.bridges.get(oldestDir);
      entry?.bridge.shutdown().catch((err) => this.error("eviction shutdown failed:", err));
      this.bridges.delete(oldestDir);
    }
  }

  /** Shut down all bridges and stop the cleanup timer. */
  async shutdown(): Promise<void> {
    if (this.cleanupTimer) {
      clearInterval(this.cleanupTimer);
      this.cleanupTimer = null;
    }
    const shutdowns = Array.from(this.bridges.values()).map((e) => e.bridge.shutdown());
    this.bridges.clear();
    await Promise.allSettled(shutdowns);
  }

  /**
   * Replace the binary path and restart all bridges.
   * Used after downloading a newer binary version.
   */
  async replaceBinary(newPath: string): Promise<string> {
    this.binaryPath = newPath;
    // Clear the pool so next getBridge() creates fresh bridges with the new binary.
    // Do NOT call shutdown() here: when replaceBinary() is invoked from a bridge's
    // onVersionMismatch callback, shutdown() marks that in-flight bridge as
    // shutting down before BinaryBridge.replaceCurrentBinary() can restart it,
    // breaking the transparent retry path. Existing bridge processes are left to
    // finish their current calls; the current bridge owns its coordinated restart
    // after the callback returns.
    this.bridges.clear();
    this.log(
      `Binary path updated to ${newPath}. All bridges cleared — next calls will use the new binary.`,
    );
    return newPath;
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
   * Update or set a single configure override that will be applied to every
   * **future** bridge spawn. Existing bridges keep their original configure
   * payload — this method intentionally does NOT restart them, because that
   * would discard their warm trigram/semantic/LSP/symbol-cache state. Use
   * this for opt-in features that resolve asynchronously after plugin load
   * (e.g. ONNX runtime download finishing in the background).
   *
   * If `value === undefined`, the override key is removed.
   */
  setConfigureOverride(key: string, value: unknown): void {
    if (value === undefined) {
      delete this.configOverrides[key];
    } else {
      this.configOverrides[key] = value;
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
}

/** Canonicalize bridge keys so symlinked paths and trailing separators collapse to one key. */
function normalizeKey(projectRoot: string): string {
  const stripped = projectRoot.replace(/[/\\]+$/, "");
  try {
    return realpathSync(stripped);
  } catch {
    return stripped;
  }
}
