import { type ChildProcess, spawn } from "node:child_process";
import { homedir } from "node:os";
import { join } from "node:path";
import { StringDecoder } from "node:string_decoder";

import { error, getActiveLogger, getLogFilePath, log, warn } from "./active-logger.js";
import type { Logger, LogMeta } from "./logger.js";
import type { BgCompletion, StatusCompression } from "./protocol.js";

const DEFAULT_BRIDGE_TIMEOUT_MS = 30_000;
const SEMANTIC_TIMEOUT_SAFETY_MARGIN_MS = 5_000;
const MAX_STDOUT_BUFFER = 64 * 1024 * 1024; // 64MB

// ## Note on TypeScript `as` type assertions
//
// Bridge responses use `as string`, `as string[]` etc. in several places.
// This is intentional: all 16 tool handlers already guard against error
// responses with `if (response.success === false) throw ...` before accessing
// typed fields. The remaining `as` casts are on fields from known-success
// Rust responses where the shape is guaranteed by the protocol contract.
// Adding Zod runtime validation for every bridge response would add ~2ms
// per call with no practical safety benefit given the error guards.

/**
 * Compare two semver version strings (major.minor.patch plus pre-release).
 * Returns: negative if a < b, 0 if equal, positive if a > b.
 */
/**
 * Re-tag a single stderr line forwarded from the `aft` child process.
 *
 * env_logger in `aft` emits each log line with an outer `[aft]` or `[aft-lsp]`
 * tag based on log target. The plugin logger then wraps those lines with its
 * own `[aft-plugin]` outer tag. We must NOT add a second `[aft]` here when
 * the line is already tagged, or LSP errors end up rendered as
 * `[aft-plugin] [aft] [aft-lsp] [aft] ...` (the v0.19.0 doubled-prefix bug).
 *
 * Rule:
 * - Already starts with `[aft]` or `[aft-<word>]` → leave as-is.
 * - Untagged (rare child-library output, panics, etc.) → prepend `[aft]`.
 *
 * Exported for unit testing; production callers use it inside the stderr
 * `on("data")` handler in `BinaryBridge.spawn`.
 */
export function tagStderrLine(line: string): string {
  return /^\[aft(-\w+)?\] /.test(line) ? line : `[aft] ${line}`;
}

export function compareSemver(a: string, b: string): number {
  const [aMain, aPre] = a.split("-", 2);
  const [bMain, bPre] = b.split("-", 2);
  const aParts = aMain.split(".").map(Number);
  const bParts = bMain.split(".").map(Number);
  for (let i = 0; i < 3; i++) {
    if (aParts[i] !== bParts[i]) return (aParts[i] ?? 0) - (bParts[i] ?? 0);
  }
  if (!aPre && !bPre) return 0;
  if (!aPre) return 1;
  if (!bPre) return -1;

  const aIds = aPre.split(".");
  const bIds = bPre.split(".");
  for (let i = 0; i < Math.max(aIds.length, bIds.length); i++) {
    const ai = aIds[i];
    const bi = bIds[i];
    if (ai === undefined) return -1;
    if (bi === undefined) return 1;
    const aNum = /^\d+$/.test(ai);
    const bNum = /^\d+$/.test(bi);
    if (aNum && bNum) {
      const diff = Number.parseInt(ai, 10) - Number.parseInt(bi, 10);
      if (diff !== 0) return diff;
    } else if (aNum) {
      return -1;
    } else if (bNum) {
      return 1;
    } else {
      const cmp = ai.localeCompare(bi);
      if (cmp !== 0) return cmp;
    }
  }
  return 0;
}

function clampSemanticTimeout(
  configOverrides: Record<string, unknown>,
  bridgeTimeoutMs: number,
): Record<string, unknown> {
  const semantic = configOverrides.semantic;
  if (!semantic || typeof semantic !== "object" || Array.isArray(semantic)) {
    return configOverrides;
  }

  const timeoutMs = (semantic as { timeout_ms?: unknown }).timeout_ms;
  if (typeof timeoutMs !== "number" || !Number.isFinite(timeoutMs)) {
    return configOverrides;
  }

  const maxSemanticTimeoutMs =
    bridgeTimeoutMs > SEMANTIC_TIMEOUT_SAFETY_MARGIN_MS
      ? bridgeTimeoutMs - SEMANTIC_TIMEOUT_SAFETY_MARGIN_MS
      : Math.max(1, bridgeTimeoutMs - 1);

  if (timeoutMs <= maxSemanticTimeoutMs) {
    return configOverrides;
  }

  warn(
    `semantic.timeout_ms=${timeoutMs} exceeds bridge timeout budget; clamping to ${maxSemanticTimeoutMs}ms (bridge timeout: ${bridgeTimeoutMs}ms)`,
  );

  return {
    ...configOverrides,
    semantic: {
      ...semantic,
      timeout_ms: maxSemanticTimeoutMs,
    },
  };
}

interface PendingRequest {
  resolve: (value: Record<string, unknown>) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
  onProgress?: (chunk: { kind: "stdout" | "stderr"; text: string }) => void;
}

/** Single configure-time warning produced by the Rust side. */
export interface ConfigureWarning {
  code?: string;
  message: string;
  [key: string]: unknown;
}

/** Context passed to {@link BridgeOptions.onConfigureWarnings} after the first successful configure. */
export interface ConfigureWarningsContext {
  projectRoot: string;
  sessionId?: string | null;
  client?: unknown;
  warnings: ConfigureWarning[];
}

export type VersionMismatchCallbackResult = string | null | undefined;

export type VersionMismatchCallback = (
  binaryVersion: string,
  minVersion: string,
) => VersionMismatchCallbackResult | Promise<VersionMismatchCallbackResult>;

class BridgeReplacedDuringVersionCheck extends Error {
  constructor(public readonly newBinaryPath: string) {
    super(`Bridge binary replaced during version check: ${newBinaryPath}`);
    this.name = "BridgeReplacedDuringVersionCheck";
  }
}

export interface BridgeOptions {
  /** Request timeout in milliseconds. Default: 30000 */
  timeoutMs?: number;
  /** Maximum restart attempts before giving up. Default: 3 */
  maxRestarts?: number;
  /** Minimum binary version required (semver). If the binary is older, onVersionMismatch is called. */
  minVersion?: string;
  /**
   * Called when binary version is older than minVersion. Receives (binaryVersion, minVersion).
   * Return a replacement binary path to coordinate a one-shot retry, null to abort, or void for
   * legacy fire-and-forget behavior.
   */
  onVersionMismatch?: VersionMismatchCallback;
  /** Called after the first successful configure returns user-visible warnings. */
  onConfigureWarnings?: (context: ConfigureWarningsContext) => void | Promise<void>;
  /** Called for server-pushed background bash completions. */
  onBashCompletion?: (
    completion: BashCompletedPayload,
    bridge: BinaryBridge,
  ) => void | Promise<void>;
  /** Called for server-pushed long-running bash reminders. */
  onBashLongRunning?: (
    reminder: BashLongRunningPayload,
    bridge: BinaryBridge,
  ) => void | Promise<void>;
  /**
   * Prefix for user-facing error messages thrown by the bridge (e.g. timeout,
   * stdin-write, configure-failure errors). Hosts pass their own tag so the
   * agent and operators see consistent attribution. Defaults to `[aft-bridge]`.
   */
  errorPrefix?: string;
  /**
   * Optional logger to use for this bridge's diagnostics. When omitted, the
   * bridge falls back to the active-logger singleton (set via
   * `setActiveLogger`). Matches the same option on `BridgePool` (Oracle F9) so
   * hosts that bundle aft-bridge multiple times can route logs deterministically.
   */
  logger?: Logger;
}

export interface BashCompletedPayload extends BgCompletion {
  type: "bash_completed";
  session_id: string;
}

export interface BashLongRunningPayload {
  type: "bash_long_running";
  task_id: string;
  session_id: string;
  command: string;
  elapsed_ms: number;
}

export interface StatusSnapshot {
  version?: string;
  project_root?: string | null;
  canonical_root?: string | null;
  cache_role?: "main" | "worktree" | "not_initialized" | string;
  search_index?: Record<string, unknown>;
  semantic_index?: Record<string, unknown>;
  disk?: Record<string, unknown>;
  lsp_servers?: number;
  symbol_cache?: Record<string, unknown>;
  storage_dir?: string | null;
  features?: Record<string, unknown>;
  compression?: StatusCompression;
  [key: string]: unknown;
}

export interface BridgeRequestOptions {
  onProgress?: (chunk: { kind: "stdout" | "stderr"; text: string }) => void;
  /** Per-call transport timeout in milliseconds. Defaults to the bridge-wide timeout. */
  transportTimeoutMs?: number;
  /**
   * Skip the "kill the child process on timeout" behavior for this request.
   *
   * The default (false) treats a transport-level timeout as evidence the bridge
   * is wedged — Rust normally responds well within the budget, so silence past
   * the deadline almost always means a stuck child. Killing forces a clean
   * respawn on the next call.
   *
   * Some commands enforce their own timeouts on the Rust side (notably `bash`,
   * which uses a watchdog thread to terminate the child shell and return a
   * timeout response). For those, a transport timeout means the response was
   * lost or queued behind something else — the bridge itself is still healthy
   * and should keep its warm state (LSP servers, semantic index, callers
   * cache, undo history). Pass `keepBridgeOnTimeout: true` to reject the
   * request without tearing down the bridge.
   */
  keepBridgeOnTimeout?: boolean;
}

interface SendOptions extends BridgeRequestOptions {
  timeoutMs?: number;
  configureWarningClient?: unknown;
}

/**
 * Manages a persistent `aft` child process, communicating via NDJSON over
 * stdin/stdout. Lazy-spawns on first `send()` call. Handles crash detection
 * with exponential backoff auto-restart.
 */
export class BinaryBridge {
  private static readonly RESTART_RESET_MS = 5 * 60 * 1000;
  /** How many recent stderr lines to keep for crash diagnostics. */
  private static readonly STDERR_TAIL_MAX = 20;

  private binaryPath: string;
  private cwd: string;
  private process: ChildProcess | null = null;
  private pending = new Map<string, PendingRequest>();
  private nextId = 1;
  private stdoutBuffer = "";
  private stderrBuffer = "";
  /** Ring buffer of the last N stderr lines, cleared on every spawn. */
  private stderrTail: string[] = [];
  private _restartCount = 0;
  private _shuttingDown = false;
  private timeoutMs: number;
  private maxRestarts: number;
  private configured = false;
  private _configurePromise: Promise<void> | null = null;
  private configOverrides: Record<string, unknown>;
  private minVersion: string | undefined;
  private onVersionMismatch: VersionMismatchCallback | undefined;
  private onConfigureWarnings:
    | ((context: ConfigureWarningsContext) => void | Promise<void>)
    | undefined;
  private onBashCompletion:
    | ((completion: BashCompletedPayload, bridge: BinaryBridge) => void | Promise<void>)
    | undefined;
  private onBashLongRunning:
    | ((reminder: BashLongRunningPayload, bridge: BinaryBridge) => void | Promise<void>)
    | undefined;
  private cachedStatus: StatusSnapshot | null = null;
  private statusListeners = new Set<(snapshot: StatusSnapshot) => void>();
  /** Notification clients keyed by session_id for async configure warning pushes. */
  private configureWarningClients = new Map<string, unknown>();
  private restartResetTimer: ReturnType<typeof setTimeout> | null = null;
  private errorPrefix: string;
  private readonly logger: Logger | undefined;

  constructor(
    binaryPath: string,
    cwd: string,
    options?: BridgeOptions,
    configOverrides?: Record<string, unknown>,
  ) {
    this.binaryPath = binaryPath;
    this.cwd = cwd;
    this.timeoutMs = options?.timeoutMs ?? DEFAULT_BRIDGE_TIMEOUT_MS;
    this.maxRestarts = options?.maxRestarts ?? 3;
    this.configOverrides = clampSemanticTimeout(configOverrides ?? {}, this.timeoutMs);
    this.minVersion = options?.minVersion;
    this.onVersionMismatch = options?.onVersionMismatch;
    this.onConfigureWarnings = options?.onConfigureWarnings;
    this.onBashCompletion = options?.onBashCompletion;
    this.onBashLongRunning = options?.onBashLongRunning;
    this.errorPrefix = options?.errorPrefix ?? "[aft-bridge]";
    this.logger = options?.logger;
  }

  private logVia(message: string, meta?: LogMeta): void {
    const logger = this.logger ?? getActiveLogger();
    if (logger) {
      try {
        logger.log(message, meta);
      } catch (err) {
        console.error(
          `[aft-bridge] ERROR: logger log threw: ${err instanceof Error ? err.message : String(err)}`,
        );
        console.error(`[aft-bridge] ${message}`);
      }
    } else {
      log(message, meta);
    }
  }

  private warnVia(message: string, meta?: LogMeta): void {
    const logger = this.logger ?? getActiveLogger();
    if (logger) {
      try {
        logger.warn(message, meta);
      } catch (err) {
        console.error(
          `[aft-bridge] ERROR: logger warn threw: ${err instanceof Error ? err.message : String(err)}`,
        );
        console.error(`[aft-bridge] WARN: ${message}`);
      }
    } else {
      warn(message, meta);
    }
  }

  private errorVia(message: string, meta?: LogMeta): void {
    const logger = this.logger ?? getActiveLogger();
    if (logger) {
      try {
        logger.error(message, meta);
      } catch (err) {
        console.error(
          `[aft-bridge] ERROR: logger error threw: ${err instanceof Error ? err.message : String(err)}`,
        );
        console.error(`[aft-bridge] ERROR: ${message}`);
      }
    } else {
      error(message, meta);
    }
  }

  private getLogFilePathVia(): string | undefined {
    if (this.logger?.getLogFilePath) {
      try {
        return this.logger.getLogFilePath();
      } catch (err) {
        console.error(
          `[aft-bridge] ERROR: logger getLogFilePath threw: ${err instanceof Error ? err.message : String(err)}`,
        );
        return undefined;
      }
    }
    return getLogFilePath();
  }

  private sessionLogVia(sessionId: string | undefined, message: string): void {
    this.logVia(message, sessionId ? { sessionId } : undefined);
  }

  private sessionWarnVia(sessionId: string | undefined, message: string): void {
    this.warnVia(message, sessionId ? { sessionId } : undefined);
  }

  private sessionErrorVia(sessionId: string | undefined, message: string): void {
    this.errorVia(message, sessionId ? { sessionId } : undefined);
  }

  /** Number of times the binary has been restarted after a crash. */
  get restartCount(): number {
    return this._restartCount;
  }

  /** Whether the child process is currently alive. */
  isAlive(): boolean {
    return this.process !== null && this.process.exitCode === null && !this.process.killed;
  }

  hasPendingRequests(): boolean {
    return this.pending.size > 0;
  }

  /** Returns the latest pushed or primed status snapshot, or null before the cold path completes. */
  getCachedStatus(): StatusSnapshot | null {
    return this.cachedStatus;
  }

  /**
   * Subscribe to status updates. If a snapshot is already cached, the listener
   * is invoked synchronously before this method returns. Listener errors are
   * caught and logged so one subscriber cannot break delivery to others.
   */
  subscribeStatus(listener: (snapshot: StatusSnapshot) => void): () => void {
    this.statusListeners.add(listener);
    if (this.cachedStatus !== null) {
      this.deliverStatusSnapshot(listener, this.cachedStatus);
    }
    return () => {
      this.statusListeners.delete(listener);
    };
  }

  /** Seed the plugin-side cache from the direct `status` cold path. */
  cacheStatusSnapshot(snapshot: StatusSnapshot): void {
    this.cachedStatus = snapshot;
  }

  /**
   * Send a command to the binary and return the parsed response.
   * Lazy-spawns the binary on first call.
   */
  async send(
    command: string,
    params: Record<string, unknown> = {},
    options?: SendOptions,
  ): Promise<Record<string, unknown>> {
    return this.sendWithVersionMismatchRetry(command, params, options, true);
  }

  private async sendWithVersionMismatchRetry(
    command: string,
    params: Record<string, unknown>,
    options: SendOptions | undefined,
    canRetryAfterVersionSwap: boolean,
  ): Promise<Record<string, unknown>> {
    try {
      if (this._shuttingDown) {
        throw new Error(`${this.errorPrefix} Bridge is shutting down, cannot send "${command}"`);
      }

      if (Object.hasOwn(params, "id")) {
        throw new Error("params cannot contain reserved key 'id'");
      }

      // Capture session_id BEFORE ensureSpawned so the spawn-time log line gets
      // tagged with the triggering session. Bridges are project-keyed and serve
      // many sessions over their lifetime, but the spawn itself is attributable
      // to whichever session's tool call triggered it.
      const requestSessionId =
        typeof params.session_id === "string" && params.session_id.length > 0
          ? params.session_id
          : undefined;

      this.ensureSpawned(requestSessionId);

      // Auto-configure can reuse the initiating session's notification client
      // when the deferred configure warning frame arrives later. One project
      // bridge can serve many sessions, so keep this per-session instead of one
      // bridge-wide "last client".
      if (requestSessionId && options?.configureWarningClient !== undefined) {
        this.configureWarningClients.set(requestSessionId, options.configureWarningClient);
      }

      // Auto-configure project root + plugin config on first command, then check version.
      // configured is set AFTER success to prevent skipping configuration on failure (#18).
      // When multiple parallel calls arrive before configure completes, they all await
      // the same promise instead of each independently trying to configure.
      if (!this.configured) {
        if (command !== "configure" && command !== "version") {
          if (!this._configurePromise) {
            // First caller — create the configure promise.
            // All parallel callers await this same promise.
            //
            // Forward the triggering call's session_id into configure so
            // Rust's thread-local session context propagates through to
            // background tasks spawned by configure (search-index pre-warm,
            // semantic-index build). Without this, background log lines
            // emitted by configure threads appear with no session prefix.
            const sessionIdForConfigure =
              typeof params.session_id === "string" ? (params.session_id as string) : undefined;
            this._configurePromise = (async () => {
              try {
                const configResult = await this.send("configure", {
                  project_root: this.cwd,
                  ...this.configOverrides,
                  ...(sessionIdForConfigure ? { session_id: sessionIdForConfigure } : {}),
                });
                if (configResult.success === false) {
                  throw new Error(
                    `${this.errorPrefix} Configure failed: ${configResult.message ?? "unknown error"}`,
                  );
                }
                // Large-repo warning is emitted by the Rust side via log::warn!
                // and relayed through stderr → plugin log. No need to re-log here
                // (doing so would just duplicate the same line in aft-plugin.log).
                await this.deliverConfigureWarnings(configResult, params, options);
                await this.checkVersion();
                // Re-check liveness after version check — checkVersion() swallows
                // errors as best-effort, so the bridge may have died without throwing.
                if (!this.isAlive()) {
                  throw new Error(
                    `${this.errorPrefix} Bridge died during version check. Check logs: ${this.getLogFilePathVia()}`,
                  );
                }
                this.configured = true;
              } finally {
                this._configurePromise = null;
              }
            })();
          }

          // All callers (including the first) await the shared promise
          await this._configurePromise;
        }
      }

      const id = String(this.nextId++);
      // Wire format: when params contains a key that collides with the protocol
      // envelope (`command`/`method`), nest params under a `params` key so the
      // outer dispatch dispatches on `command: "<bridge command>"` rather than
      // the user's payload key. Reserved envelope fields (`session_id`,
      // `lsp_hints`) must STILL be promoted to the top level so RawRequest's
      // dedicated fields deserialize correctly. Without this promotion, e.g.
      // `bash` (whose params include `command: "<shell command>"`) silently
      // loses `session_id` because it stays nested inside `params`.
      let request: Record<string, unknown>;
      if (Object.hasOwn(params, "command") || Object.hasOwn(params, "method")) {
        const nested: Record<string, unknown> = { ...params };
        const reserved: Record<string, unknown> = {};
        for (const key of ["session_id", "lsp_hints"] as const) {
          if (Object.hasOwn(nested, key)) {
            reserved[key] = nested[key];
            delete nested[key];
          }
        }
        request = { id, command, ...reserved, params: nested };
      } else {
        request = { id, command, ...params };
      }
      const line = `${JSON.stringify(request)}\n`;

      // Per-op timeout override: tool wrappers can pass longer budgets for
      // commands that legitimately need them (callers, trace_to, grep on big
      // repos). Defaults to the bridge-wide timeout otherwise.
      const effectiveTimeoutMs =
        options?.transportTimeoutMs ?? options?.timeoutMs ?? this.timeoutMs;

      const keepBridgeOnTimeout = options?.keepBridgeOnTimeout === true;

      return new Promise<Record<string, unknown>>((resolve, reject) => {
        const timer = setTimeout(() => {
          this.pending.delete(id);
          const restartSuffix = keepBridgeOnTimeout ? "" : " — restarting bridge";
          const timeoutMsg = `Request "${command}" (id=${id}) timed out after ${effectiveTimeoutMs}ms${restartSuffix}`;
          if (requestSessionId) {
            this.sessionWarnVia(requestSessionId, timeoutMsg);
          } else {
            this.warnVia(timeoutMsg);
          }
          reject(
            new Error(
              `${this.errorPrefix} Request "${command}" (id=${id}) timed out after ${effectiveTimeoutMs}ms`,
            ),
          );
          // Kill the hung process so the next request gets a fresh bridge —
          // unless the caller explicitly opted out (e.g. bash, which enforces
          // its own timeout on the Rust side and shouldn't lose warm bridge
          // state when its response is merely late).
          if (!keepBridgeOnTimeout) {
            this.handleTimeout(requestSessionId);
          }
        }, effectiveTimeoutMs);

        this.pending.set(id, { resolve, reject, timer, onProgress: options?.onProgress });

        if (!this.process?.stdin?.writable) {
          this.pending.delete(id);
          clearTimeout(timer);
          reject(new Error(`${this.errorPrefix} stdin not writable for command "${command}"`));
          return;
        }

        this.process.stdin.write(line, (err) => {
          if (err) {
            const entry = this.pending.get(id);
            if (entry) {
              this.pending.delete(id);
              clearTimeout(entry.timer);
              entry.reject(
                new Error(`${this.errorPrefix} Failed to write to stdin: ${err.message}`),
              );
            }
          }
        });
      });
    } catch (err) {
      if (
        err instanceof BridgeReplacedDuringVersionCheck &&
        canRetryAfterVersionSwap &&
        command !== "configure" &&
        command !== "version"
      ) {
        this.logVia(
          `Retrying request "${command}" once after coordinated binary replacement: ${err.newBinaryPath}`,
        );
        return this.sendWithVersionMismatchRetry(command, params, options, false);
      }
      throw err;
    }
  }

  private async deliverConfigureWarnings(
    configResult: Record<string, unknown>,
    params: Record<string, unknown>,
    options: SendOptions | undefined,
  ): Promise<void> {
    if (!this.onConfigureWarnings || !Array.isArray(configResult.warnings)) return;
    if (configResult.warnings.length === 0) return;

    const sessionId = typeof params.session_id === "string" ? params.session_id : undefined;
    try {
      await this.onConfigureWarnings({
        projectRoot: this.cwd,
        sessionId,
        client:
          options?.configureWarningClient ??
          (sessionId ? this.configureWarningClients.get(sessionId) : undefined),
        warnings: configResult.warnings,
      });
    } catch (err) {
      this.warnVia(
        `configure warning delivery failed: ${err instanceof Error ? err.message : String(err)}`,
      );
    } finally {
      if (sessionId) {
        this.configureWarningClients.delete(sessionId);
      }
    }
  }

  /**
   * Handle the `configure_warnings` push frame the Rust binary emits after
   * configure has returned. The frame carries the warnings produced by the
   * deferred file walk + missing-binary detection. Forwards to the same
   * `onConfigureWarnings` handler used for synchronous warnings so plugins
   * don't need to know about the async path.
   */
  private async handleConfigureWarningsFrame(frame: Record<string, unknown>): Promise<void> {
    if (!this.onConfigureWarnings) return;
    const warnings = frame.warnings;
    if (!Array.isArray(warnings) || warnings.length === 0) return;
    const projectRoot = typeof frame.project_root === "string" ? frame.project_root : this.cwd;
    const rawSessionId = frame.session_id;
    const sessionId =
      typeof rawSessionId === "string" && rawSessionId.length > 0 ? rawSessionId : null;
    try {
      await this.onConfigureWarnings({
        projectRoot,
        sessionId,
        client: sessionId ? this.configureWarningClients.get(sessionId) : undefined,
        warnings: warnings as ConfigureWarning[],
      });
    } finally {
      if (sessionId) {
        this.configureWarningClients.delete(sessionId);
      }
    }
  }

  private handleStatusChangedFrame(frame: Record<string, unknown>): void {
    const snapshot = frame.snapshot;
    if (!snapshot || typeof snapshot !== "object" || Array.isArray(snapshot)) return;
    this.cachedStatus = snapshot as StatusSnapshot;
    this.logVia("Received status_changed push frame; cached AFT status snapshot");
    for (const listener of this.statusListeners) {
      this.deliverStatusSnapshot(listener, this.cachedStatus);
    }
  }

  private deliverStatusSnapshot(
    listener: (snapshot: StatusSnapshot) => void,
    snapshot: StatusSnapshot,
  ): void {
    try {
      listener(snapshot);
    } catch (err) {
      this.warnVia(`status listener threw: ${err instanceof Error ? err.message : String(err)}`);
    }
  }

  /** Kill the child process and reject all pending requests. */
  async shutdown(): Promise<void> {
    this._shuttingDown = true;
    this.clearRestartResetTimer();
    this.configureWarningClients.clear();
    this.rejectAllPending(new Error(`${this.errorPrefix} Bridge shutting down`));

    if (this.process) {
      const proc = this.process;
      this.process = null;

      return new Promise<void>((resolve) => {
        const forceKillTimer = setTimeout(() => {
          proc.kill("SIGKILL");
          resolve();
        }, 5_000);

        proc.once("exit", () => {
          clearTimeout(forceKillTimer);
          this.logVia("Process exited during shutdown");
          resolve();
        });

        proc.kill("SIGTERM");
      });
    }
  }

  // ---- Internal ----

  /** Query binary version and compare against minVersion. Calls onVersionMismatch if outdated. */
  private async checkVersion(): Promise<void> {
    if (!this.minVersion) return;
    try {
      const resp = await this.send("version");
      if (resp.success === false) {
        throw new Error(
          `Binary version check failed: ${String(resp.code ?? "unknown")} — likely too old`,
        );
      }
      const binaryVersion = resp.version as string | undefined;
      if (typeof binaryVersion !== "string") {
        throw new Error(
          `Binary did not report a version — likely too old (minVersion: ${this.minVersion})`,
        );
      }
      this.logVia(`Binary version: ${binaryVersion}`);
      if (compareSemver(binaryVersion, this.minVersion) < 0) {
        this.warnVia(`Binary version ${binaryVersion} is older than required ${this.minVersion}`);
        const replacementPath = await this.onVersionMismatch?.(binaryVersion, this.minVersion);
        if (replacementPath === undefined) {
          // Backwards compatibility: legacy callbacks returned void and usually kicked off a
          // fire-and-forget download + pool swap. Preserve that behavior for existing callers.
          return;
        }
        if (replacementPath === null || replacementPath.length === 0) {
          throw new Error(
            `Binary version ${binaryVersion} is older than required ${this.minVersion}; no compatible replacement binary was provided`,
          );
        }

        await this.replaceCurrentBinary(replacementPath);
        throw new BridgeReplacedDuringVersionCheck(replacementPath);
      }
    } catch (err) {
      this.warnVia(`Version check failed: ${(err as Error).message}`);
      throw err;
    }
  }

  private async replaceCurrentBinary(newBinaryPath: string): Promise<void> {
    this.binaryPath = newBinaryPath;
    this.configured = false;
    this.clearRestartResetTimer();
    this.rejectAllPending(
      new Error(`${this.errorPrefix} Bridge restarting with updated binary: ${newBinaryPath}`),
    );

    if (!this.process) return;

    const proc = this.process;
    this.process = null;

    await new Promise<void>((resolve) => {
      const forceKillTimer = setTimeout(() => {
        proc.kill("SIGKILL");
        resolve();
      }, 5_000);

      proc.once("exit", () => {
        clearTimeout(forceKillTimer);
        this.logVia("Process exited during coordinated binary replacement");
        resolve();
      });

      proc.kill("SIGTERM");
    });
  }

  private ensureSpawned(triggeringSessionId?: string): void {
    if (this.isAlive()) return;
    this.spawnProcess(triggeringSessionId);
  }

  private spawnProcess(triggeringSessionId?: string): void {
    if (triggeringSessionId) {
      this.sessionLogVia(
        triggeringSessionId,
        `Spawning binary: ${this.binaryPath} (cwd: ${this.cwd})`,
      );
    } else {
      this.logVia(`Spawning binary: ${this.binaryPath} (cwd: ${this.cwd})`);
    }
    const semantic = this.configOverrides.semantic;
    const semanticBackend = (() => {
      if (semantic && typeof semantic === "object" && !Array.isArray(semantic)) {
        const candidate = (semantic as { backend?: unknown }).backend;
        return typeof candidate === "string" ? candidate : undefined;
      }
      return undefined;
    })();
    const useFastembedBackend =
      semanticBackend === undefined || semanticBackend === "fastembed" || semanticBackend === "";

    const ortDir =
      typeof this.configOverrides._ort_dylib_dir === "string" && useFastembedBackend
        ? this.configOverrides._ort_dylib_dir
        : null;
    const ortLibraryPath =
      ortDir == null
        ? null
        : join(
            ortDir,
            process.platform === "win32"
              ? "onnxruntime.dll"
              : process.platform === "darwin"
                ? "libonnxruntime.dylib"
                : "libonnxruntime.so",
          );
    const envPath =
      process.platform === "win32" && ortDir
        ? `${ortDir};${process.env.PATH ?? ""}`
        : process.env.PATH;

    const env: NodeJS.ProcessEnv = {
      ...process.env,
      ...(envPath ? { PATH: envPath } : {}),
    };

    if (useFastembedBackend) {
      // Store fastembed model files alongside the semantic index, not the project cwd.
      // This is only relevant when the fastembed backend is selected.
      env.FASTEMBED_CACHE_DIR =
        process.env.FASTEMBED_CACHE_DIR ||
        (typeof this.configOverrides.storage_dir === "string"
          ? join(this.configOverrides.storage_dir, "semantic", "models")
          : join(homedir() || "", ".cache", "fastembed"));

      // Point ort to the auto-downloaded or system ONNX Runtime library.
      if (ortLibraryPath) {
        env.ORT_DYLIB_PATH = ortLibraryPath;
      }
    }

    const child = spawn(this.binaryPath, [], {
      cwd: this.cwd,
      stdio: ["pipe", "pipe", "pipe"],
      env,
    });
    const currentChild = child;

    const stdoutDecoder = new StringDecoder("utf8");
    child.stdout?.on("data", (chunk: Buffer) => {
      this.onStdoutData(stdoutDecoder.write(chunk));
    });
    child.stdout?.on("end", () => {
      const remaining = stdoutDecoder.end();
      if (remaining) this.onStdoutData(remaining);
    });

    const stderrDecoder = new StringDecoder("utf8");
    child.stderr?.on("data", (chunk: Buffer) => {
      this.onStderrData(stderrDecoder.write(chunk));
    });
    child.stderr?.on("end", () => {
      const remaining = stderrDecoder.end();
      if (remaining) this.onStderrData(remaining);
      this.flushStderrBuffer();
    });

    child.on("error", (err) => {
      if (this.process !== currentChild) return;
      this.errorVia(`Process error: ${err.message}${this.formatStderrTail()}`);
      this.handleCrash();
    });

    child.on("exit", (code, signal) => {
      if (this.process !== currentChild) return;
      if (this._shuttingDown) return;
      this.logVia(`Process exited: code=${code}, signal=${signal}`);
      // External termination signals (SIGTERM/SIGKILL/SIGHUP/SIGINT) are almost
      // always intentional kills — from our own shutdown path, OpenCode tearing
      // down, OS shutdown, or the user killing the host. Auto-restarting here
      // produces process avalanches (issue #14): N bridges all receive SIGTERM
      // simultaneously, each "auto-restarts", spawning N fresh processes that
      // reload ONNX + semantic + trigram indexes. Real Rust panics/crashes exit
      // with a non-null `code` and `signal === null`; those still restart.
      if (
        signal === "SIGTERM" ||
        signal === "SIGKILL" ||
        signal === "SIGHUP" ||
        signal === "SIGINT"
      ) {
        this.process = null;
        this.configured = false;
        this.clearRestartResetTimer();
        this.rejectAllPending(new Error(`${this.errorPrefix} Binary killed by ${signal}`));
        return;
      }
      this.handleCrash();
    });

    this.process = child;
    this.stdoutBuffer = "";
    this.stderrBuffer = "";
    // Fresh spawn — clear the stderr ring so crash diagnostics only reflect
    // the current child's output, not output from prior restart cycles.
    this.stderrTail = [];
  }

  private pushStderrLine(line: string): void {
    this.stderrTail.push(line);
    if (this.stderrTail.length > BinaryBridge.STDERR_TAIL_MAX) {
      this.stderrTail.shift();
    }
  }

  private onStderrData(data: string): void {
    this.stderrBuffer += data;
    let newlineIdx: number;
    while ((newlineIdx = this.stderrBuffer.indexOf("\n")) !== -1) {
      const line = this.stderrBuffer.slice(0, newlineIdx).replace(/\r$/, "");
      this.stderrBuffer = this.stderrBuffer.slice(newlineIdx + 1);
      if (!line) continue;
      const tagged = tagStderrLine(line);
      this.logVia(tagged);
      this.pushStderrLine(tagged);
    }
  }

  private flushStderrBuffer(): void {
    const line = this.stderrBuffer.replace(/\r$/, "");
    this.stderrBuffer = "";
    if (!line) return;
    const tagged = tagStderrLine(line);
    this.logVia(tagged);
    this.pushStderrLine(tagged);
  }

  /**
   * Format the current stderr tail for inclusion in error messages. Returns
   * empty string when nothing has been captured (e.g., silent SIGKILL from
   * macOS amfid) so the caller can safely concatenate unconditionally.
   */
  private formatStderrTail(): string {
    if (this.stderrTail.length === 0) return "";
    const tail = this.stderrTail.join("\n  ");
    return `\n  --- last ${this.stderrTail.length} stderr lines ---\n  ${tail}`;
  }

  private onStdoutData(data: string): void {
    this.stdoutBuffer += data;
    if (this.stdoutBuffer.length > MAX_STDOUT_BUFFER) {
      this.handleCrash(
        new Error(`aft bridge stdout buffer exceeded ${MAX_STDOUT_BUFFER} bytes — killing bridge`),
      );
      return;
    }

    // Process complete lines
    let newlineIdx: number;
    while ((newlineIdx = this.stdoutBuffer.indexOf("\n")) !== -1) {
      const line = this.stdoutBuffer.slice(0, newlineIdx).trim();
      this.stdoutBuffer = this.stdoutBuffer.slice(newlineIdx + 1);

      if (!line) continue;

      try {
        const response = JSON.parse(line) as Record<string, unknown>;
        if (response.type === "progress") {
          const requestId = response.request_id as string | undefined;
          const entry = requestId ? this.pending.get(requestId) : undefined;
          const kind = response.kind === "stderr" ? "stderr" : "stdout";
          const text = typeof response.chunk === "string" ? response.chunk : "";
          entry?.onProgress?.({ kind, text });
          continue;
        }
        if (response.type === "permission_ask") {
          const requestId = response.request_id as string | undefined;
          const entry = requestId ? this.pending.get(requestId) : undefined;
          if (requestId && entry) {
            this.pending.delete(requestId);
            clearTimeout(entry.timer);
            entry.resolve({
              success: false,
              code: "permission_required",
              message: "bash command requires permission",
              asks: response.asks,
            });
          }
          continue;
        }
        if (response.type === "bash_completed") {
          this.onBashCompletion?.(response as unknown as BashCompletedPayload, this);
          continue;
        }
        if (response.type === "bash_long_running") {
          this.onBashLongRunning?.(response as unknown as BashLongRunningPayload, this);
          continue;
        }
        if (response.type === "configure_warnings") {
          this.handleConfigureWarningsFrame(response).catch((err) => {
            this.warnVia(
              `configure warning delivery failed: ${err instanceof Error ? err.message : String(err)}`,
            );
          });
          continue;
        }
        if (response.type === "status_changed") {
          this.handleStatusChangedFrame(response);
          continue;
        }
        const id = response.id as string | undefined;
        if (id && this.pending.has(id)) {
          const entry = this.pending.get(id);
          if (!entry) continue;
          this.pending.delete(id);
          clearTimeout(entry.timer);
          this.scheduleRestartCountReset();
          entry.resolve(response);
        } else if (typeof response.type === "string") {
          this.logVia(`Ignoring unknown stdout push frame type: ${response.type}`);
        }
      } catch (_err) {
        this.warnVia(`Failed to parse stdout line: ${line}`);
      }
    }
  }

  private handleTimeout(triggeringSessionId?: string): void {
    // A timed-out request means the child is about to be SIGKILLed. Reject all
    // sibling in-flight requests now instead of leaving them parked until their
    // own independent timers fire.
    this.rejectAllPending(
      new Error(`${this.errorPrefix} bridge killed during sibling timeout — request aborted`),
    );
    if (this.process) {
      this.process.kill("SIGKILL");
      this.process = null;
    }
    this.clearRestartResetTimer();
    this.configured = false;

    // Capture the stderr tail for diagnostics. The tail goes to the plugin
    // log only — it's operator-facing noise (loaded N backups, invalidated K
    // files, etc.) that the agent can't act on, so we don't put it in the
    // rejection error. Clear the ring so the next spawn doesn't inherit it.
    const tail = this.formatStderrTail();
    this.stderrTail = [];
    const killedMsg = tail
      ? `Bridge killed after timeout.${tail}`
      : `Bridge killed after timeout (see ${this.getLogFilePathVia()})`;
    if (tail) {
      if (triggeringSessionId) {
        this.sessionErrorVia(triggeringSessionId, killedMsg);
      } else {
        this.errorVia(killedMsg);
      }
    } else if (triggeringSessionId) {
      this.sessionWarnVia(triggeringSessionId, killedMsg);
    } else {
      this.warnVia(killedMsg);
    }
  }

  private handleCrash(cause?: Error): void {
    const proc = this.process;
    this.process = null;
    if (proc && proc.exitCode === null && !proc.killed) {
      proc.kill("SIGKILL");
    }
    this.clearRestartResetTimer();
    this.configured = false; // Force reconfigure on next command after restart

    // Capture the tail BEFORE spawning the replacement, because the next spawn
    // clears the ring. The tail goes to the plugin log only — it's operator
    // diagnostic output that the agent can't act on. The pending-request
    // rejection only carries a pointer to the log.
    const tail = this.formatStderrTail();
    if (tail) {
      this.errorVia(
        `Binary crashed (restarts: ${this._restartCount})${cause ? `: ${cause.message}` : ""}.${tail}`,
      );
    }

    this.rejectAllPending(
      new Error(
        `${this.errorPrefix} Binary crashed (restarts: ${this._restartCount})${cause ? `: ${cause.message}` : ""} (see ${this.getLogFilePathVia()})`,
      ),
    );

    // Auto-restart with exponential backoff
    if (this._restartCount < this.maxRestarts) {
      const delay = 100 * 2 ** this._restartCount; // 100ms, 200ms, 400ms
      this._restartCount++;
      this.logVia(`Auto-restart #${this._restartCount} in ${delay}ms`);

      setTimeout(() => {
        if (!this._shuttingDown && !this.isAlive()) {
          try {
            this.spawnProcess();
          } catch (err) {
            this.errorVia(`Failed to restart: ${(err as Error).message}`);
          }
        }
      }, delay);
      // Also decay the counter over time so repeated crashes without any
      // successful response don't permanently wedge the bridge.
      this.scheduleRestartCountReset();
    } else {
      this.errorVia(
        `Max restarts (${this.maxRestarts}) reached, giving up. Logs: ${this.getLogFilePathVia()}${tail}`,
      );
      this.scheduleRestartCountReset();
    }
  }

  private rejectAllPending(error: Error): void {
    for (const [_id, entry] of this.pending) {
      clearTimeout(entry.timer);
      entry.reject(error);
    }
    this.pending.clear();
  }

  private scheduleRestartCountReset(): void {
    this.clearRestartResetTimer();
    this.restartResetTimer = setTimeout(() => {
      this._restartCount = 0;
      this.restartResetTimer = null;
    }, BinaryBridge.RESTART_RESET_MS);
  }

  private clearRestartResetTimer(): void {
    if (this.restartResetTimer) {
      clearTimeout(this.restartResetTimer);
      this.restartResetTimer = null;
    }
  }
}
