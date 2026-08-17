import { type ChildProcess, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { StringDecoder } from "node:string_decoder";

import { error, getActiveLogger, getLogFilePath, log, warn } from "./active-logger.js";
import { isPassiveCommand, PASSIVE_COMMAND_TIMEOUT_MS } from "./command-timeouts.js";
import type { Logger, LogMeta } from "./logger.js";
import type { BgCompletion, StatusCompression } from "./protocol.js";
import type {
  AftProjectTransport,
  ToolCallArguments,
  ToolCallOptions,
  ToolCallResult,
} from "./transport.js";

const DEFAULT_BRIDGE_TIMEOUT_MS = 30_000;
const BRIDGE_HANG_TIMEOUT_THRESHOLD = 2;
const MAX_STDOUT_BUFFER = 64 * 1024 * 1024; // 64MB
const STDOUT_BUFFER_COMPACT_THRESHOLD = 64 * 1024;
const HASHLINE_REGISTRATION_LOG_INTERVAL_MS = 60_000;
const HASHLINE_REGISTRATION_LOG_STATE_LIMIT = 256;
const TERMINAL_BASH_STATUSES = new Set([
  "completed",
  "failed",
  "killed",
  "timed_out",
  // Historical/defensive aliases seen in plugin-side compatibility code.
  "cancelled",
  "timeout",
]);

function isTerminalBashStatus(status: unknown): boolean {
  return typeof status === "string" && TERMINAL_BASH_STATUSES.has(status);
}

function bashTaskIdFrom(response: Record<string, unknown>): string | undefined {
  const snakeCase = response.task_id;
  if (typeof snakeCase === "string" && snakeCase.length > 0) return snakeCase;
  const camelCase = response.taskId;
  if (typeof camelCase === "string" && camelCase.length > 0) return camelCase;
  return undefined;
}

type BinaryFingerprintReader = (binaryPath: string) => string | null | undefined;

function hashBinaryOnDisk(binaryPath: string): string | null {
  try {
    return createHash("sha256").update(readFileSync(binaryPath)).digest("hex");
  } catch {
    return null;
  }
}

let binaryFingerprintReader: BinaryFingerprintReader = hashBinaryOnDisk;

function readBinaryFingerprint(binaryPath: string): string | null {
  try {
    const fingerprint = binaryFingerprintReader(binaryPath);
    return typeof fingerprint === "string" && fingerprint.length > 0 ? fingerprint : null;
  } catch {
    return null;
  }
}

/** Test seam: replace the on-disk binary fingerprint reader without fs module mocks. */
export function __setBinaryFingerprintForTests(impl: BinaryFingerprintReader | null): void {
  binaryFingerprintReader = impl ?? hashBinaryOnDisk;
}

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

const BENIGN_CPUINFO_PROC_CPUINFO_PARSE_FAILURE =
  "failed to parse processor information from /proc/cpuinfo";

/**
 * Return false only for the known benign third-party cpuinfo line emitted by
 * ONNX Runtime's bundled pytorch/cpuinfo library in restricted Linux sandboxes.
 * This line is not an AFT failure; all other child stderr must still surface.
 */
export function shouldSurfaceStderrLine(line: string): boolean {
  const normalized = line.trim();
  return !(
    normalized === `Error in cpuinfo: ${BENIGN_CPUINFO_PROC_CPUINFO_PARSE_FAILURE}` ||
    normalized === BENIGN_CPUINFO_PROC_CPUINFO_PARSE_FAILURE
  );
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

interface PendingRequest {
  resolve: (value: Record<string, unknown>) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
  onProgress?: (chunk: { kind: "stdout" | "stderr"; text: string }) => void;
  command: string;
}

/** Single configure-time warning produced by the Rust side. */
export interface ConfigureWarning {
  code?: string;
  message: string;
  [key: string]: unknown;
}

/** Project/user trust-boundary key dropped by Rust config resolution. */
export interface ConfigureDroppedKey {
  key: string;
  tier: string;
  reason: string;
}

/** Context passed to {@link BridgeOptions.onConfigureWarnings} after the first successful configure. */
export interface ConfigureWarningsContext {
  projectRoot: string;
  sessionId?: string | null;
  client?: unknown;
  warnings: ConfigureWarning[];
  configDroppedKeys?: ConfigureDroppedKey[];
}

function coerceConfigureDroppedKeys(value: unknown): ConfigureDroppedKey[] {
  if (!Array.isArray(value)) return [];
  const dropped: ConfigureDroppedKey[] = [];
  for (const item of value) {
    if (!item || typeof item !== "object" || Array.isArray(item)) continue;
    const record = item as Record<string, unknown>;
    if (
      typeof record.key === "string" &&
      typeof record.tier === "string" &&
      typeof record.reason === "string"
    ) {
      dropped.push({ key: record.key, tier: record.tier, reason: record.reason });
    }
  }
  return dropped;
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

/**
 * Thrown when a request times out at the transport layer but the bridge is
 * being kept warm (passive/bash-family calls with `keepBridgeOnTimeout`). The
 * timeout means the bridge was *busy*, not hung — the request can be retried.
 * Carries a machine-readable `code` so pollers (e.g. the bash_watch poll loop)
 * can distinguish "bridge busy, retry" from a genuine command failure without
 * string-matching the message.
 */
export class BridgeTransportTimeoutError extends Error {
  readonly code = "transport_timeout" as const;
  constructor(
    public readonly command: string,
    public readonly timeoutMs: number,
    message: string,
  ) {
    super(message);
    this.name = "BridgeTransportTimeoutError";
  }
}

/** A standalone bridge cannot carry a request because its process transport died. */
export class BridgeTransportUnavailableError extends Error {
  readonly code = "bridge_transport_unavailable" as const;

  constructor(message: string, options?: { cause?: unknown }) {
    super(message, options);
    this.name = "BridgeTransportUnavailableError";
  }
}

/** The transport failed after a request may have been written to the child. */
export class BridgeTransportUnknownOutcomeError extends BridgeTransportUnavailableError {
  constructor(message: string, options?: { cause?: unknown }) {
    super(message, options);
    this.name = "BridgeTransportUnknownOutcomeError";
  }
}

/** Type guard for a transport-timeout rejection (bridge busy, retryable). */
export function isBridgeTransportTimeout(err: unknown): err is BridgeTransportTimeoutError {
  return err instanceof Error && (err as { code?: unknown }).code === "transport_timeout";
}

export interface BridgeOptions {
  /** Request timeout in milliseconds. Default: 30000 */
  timeoutMs?: number;
  /**
   * Consecutive silent request timeouts (no id-matched response) before the
   * bridge is killed and respawned. Default: 2. Child stdout activity since
   * the request still keeps the bridge warm regardless of this counter.
   */
  hangThreshold?: number;
  /**
   * Extra environment variables to set on the spawned `aft` child process,
   * applied on top of the inherited `process.env` at spawn time. Use this to
   * scope per-bridge child env (e.g. `AFT_CACHE_DIR` in tests) WITHOUT mutating
   * the shared process-global `process.env` — mutating `process.env` races
   * across concurrent bridges and, because spawn is lazy (first `send()`), is
   * easily restored before the child actually inherits it. A value of
   * `undefined` deletes the key from the child env.
   */
  childEnv?: Record<string, string | undefined>;
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
  /** Called when a registered bash_watch pattern matches on stdout/stderr. */
  onBashPatternMatch?: (frame: BashPatternMatchFrame, bridge: BinaryBridge) => void | Promise<void>;
  /** Prefix for error messages. Default: "[aft-bridge]" */
  errorPrefix?: string;
  /** Optional structured logger; falls back to active logger / console. */
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

export interface BashPatternMatchFrame {
  type: "bash_pattern_match";
  task_id: string;
  session_id: string;
  watch_id: string;
  match_text: string;
  match_offset: number;
  context: string;
  once: boolean;
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
  runtime?: {
    live_watchers?: number;
    live_actor_roots?: number;
    open_routes?: number;
  };
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
   * Skip bridge-hang escalation for this request.
   *
   * The default (false) treats a transport-level timeout as a possible bridge
   * hang. The bridge now escalates cautiously: a single timeout while the child
   * is still emitting stdout, or before the hang threshold is reached, rejects
   * only that request and keeps warm state. Repeated silent timeouts still kill
   * the child so the next call gets a fresh bridge.
   *
   * Some commands enforce their own timeouts on the Rust side (notably `bash`,
   * which uses a watchdog thread to terminate the child shell and return a
   * timeout response). For those, a transport timeout means the response was
   * lost or queued behind something else — the bridge itself is still healthy
   * and should keep its warm state (LSP servers, semantic index, callers
   * cache, undo history). Pass `keepBridgeOnTimeout: true` to reject the
   * request without contributing to hang escalation.
   */
  keepBridgeOnTimeout?: boolean;
}

interface SendOptions extends BridgeRequestOptions {
  timeoutMs?: number;
  configureWarningClient?: unknown;
  markConfiguredOnSuccess?: boolean;
}

/**
 * Manages a persistent `aft` child process, communicating via NDJSON over
 * stdin/stdout. Lazy-spawns on first `send()` call. Handles crash detection
 * with exponential backoff auto-restart.
 */
export class BinaryBridge implements AftProjectTransport {
  private static readonly RESTART_RESET_MS = 5 * 60 * 1000;
  /** How many recent stderr lines to keep for crash diagnostics. */
  private static readonly STDERR_TAIL_MAX = 20;

  private binaryPath: string;
  private cwd: string;
  private process: ChildProcess | null = null;
  /** Fingerprint of the on-disk binary contents captured when this child spawned. */
  private spawnedBinaryFingerprint: string | null = null;
  /** Last idle-window binary refresh check for this live child. */
  private lastBinaryFingerprintCheckAt = 0;
  /** Once true, this bridge must not accept new requests and should be drained. */
  private _retiringDueToBinaryChange = false;
  private pending = new Map<string, PendingRequest>();
  private outstandingBackgroundTaskIds = new Set<string>();
  private nextId = 1;
  private stdoutBuffer = "";
  private stdoutReadOffset = 0;
  private stderrBuffer = "";
  /** Ring buffer of the last N stderr lines, cleared on every spawn. */
  private stderrTail: string[] = [];
  private _restartCount = 0;
  private _shuttingDown = false;
  private timeoutMs: number;
  private hangThreshold: number;
  private maxRestarts: number;
  private configured = false;
  private _configurePromise: Promise<void> | null = null;
  private configOverrides: Record<string, unknown>;
  private editSlotSurvives: boolean | undefined;
  private editSlotSurvivesCaptured = false;
  private readonly hashlineRegistrationLogState = new Map<
    string,
    { lastEmittedAt: number; suppressed: number }
  >();
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
  private onBashPatternMatch:
    | ((frame: BashPatternMatchFrame, bridge: BinaryBridge) => void | Promise<void>)
    | undefined;
  private cachedStatus: StatusSnapshot | null = null;
  private statusListeners = new Set<(snapshot: StatusSnapshot) => void>();
  /** Notification clients keyed by session_id for async configure warning pushes. */
  private configureWarningClients = new Map<string, unknown>();
  private restartResetTimer: ReturnType<typeof setTimeout> | null = null;
  /** Updated after every successfully parsed stdout frame from the child. */
  private lastChildActivityAt = 0;
  /** Consecutive non-bash-style request timeouts without an id-matched response. */
  private consecutiveRequestTimeouts = 0;
  private errorPrefix: string;
  private readonly logger: Logger | undefined;
  private readonly childEnv: Record<string, string | undefined> | undefined;

  constructor(
    binaryPath: string,
    cwd: string,
    options?: BridgeOptions,
    configOverrides?: Record<string, unknown>,
    editSlotSurvives?: boolean,
  ) {
    this.binaryPath = binaryPath;
    this.cwd = cwd;
    this.timeoutMs = options?.timeoutMs ?? DEFAULT_BRIDGE_TIMEOUT_MS;
    this.hangThreshold = options?.hangThreshold ?? BRIDGE_HANG_TIMEOUT_THRESHOLD;
    this.maxRestarts = options?.maxRestarts ?? 3;
    this.errorPrefix = options?.errorPrefix ?? "[aft-bridge]";
    // P1 config relocation: semantic config now arrives as raw `config` tiers and
    // is resolved (incl. timeout clamping to MAX_SEMANTIC_TIMEOUT_MS) in AFT-core,
    // not here. The old bridge-side clampSemanticTimeout keyed off a flat `semantic`
    // param the plugins no longer send, so it was a dead no-op — removed. If the
    // query-embed ever needs a transport-budget race-guard it belongs at query time
    // in Rust, not as a configure-time clamp here.
    this.configOverrides = { ...(configOverrides ?? {}) };
    const legacyEditSlotSurvives = this.configOverrides.edit_slot_survives;
    delete this.configOverrides.edit_slot_survives;
    if (legacyEditSlotSurvives !== undefined && typeof legacyEditSlotSurvives !== "boolean") {
      throw new Error(`${this.errorPrefix} edit_slot_survives must be a boolean`);
    }
    if (
      editSlotSurvives !== undefined &&
      legacyEditSlotSurvives !== undefined &&
      editSlotSurvives !== legacyEditSlotSurvives
    ) {
      throw new Error(`${this.errorPrefix} conflicting edit_slot_survives construction values`);
    }
    const capturedEditSlotSurvives = editSlotSurvives ?? legacyEditSlotSurvives;
    if (typeof capturedEditSlotSurvives === "boolean") {
      this.editSlotSurvives = capturedEditSlotSurvives;
      this.editSlotSurvivesCaptured = true;
    }
    this.minVersion = options?.minVersion;
    this.onVersionMismatch = options?.onVersionMismatch;
    this.onConfigureWarnings = options?.onConfigureWarnings;
    this.onBashCompletion = options?.onBashCompletion;
    this.onBashLongRunning = options?.onBashLongRunning;
    this.onBashPatternMatch = options?.onBashPatternMatch;
    this.logger = options?.logger;
    this.childEnv = options?.childEnv;
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

  private invalidateTransportProcess(error: BridgeTransportUnavailableError): void {
    const proc = this.process;
    if (!proc) return;

    this.process = null;
    this.spawnedBinaryFingerprint = null;
    if (proc.exitCode === null && !proc.killed) {
      proc.kill("SIGKILL");
    }
    this.clearRestartResetTimer();
    this.configured = false;
    this.outstandingBackgroundTaskIds.clear();
    this.rejectAllPending(
      error instanceof BridgeTransportUnknownOutcomeError
        ? error
        : new BridgeTransportUnknownOutcomeError(error.message, { cause: error }),
    );
  }

  hasPendingRequests(): boolean {
    return this.pending.size > 0;
  }

  hasOutstandingBackgroundTasks(): boolean {
    return this.outstandingBackgroundTaskIds.size > 0;
  }

  /**
   * Idle-window maintenance hook: when the on-disk binary changed since this
   * child spawned, stop routing new work to this bridge so the pool can let it
   * drain and retire it.
   */
  maybeScheduleRespawnForUpdatedBinary(checkIntervalMs: number, now = Date.now()): boolean {
    if (this._shuttingDown || this._retiringDueToBinaryChange || !this.isAlive()) return false;
    if (this.pending.size > 0 || this.outstandingBackgroundTaskIds.size > 0) return false;
    if (now - this.lastBinaryFingerprintCheckAt < checkIntervalMs) return false;
    this.lastBinaryFingerprintCheckAt = now;

    const spawnedFingerprint = this.spawnedBinaryFingerprint;
    if (!spawnedFingerprint) return false;

    const currentFingerprint = readBinaryFingerprint(this.binaryPath);
    if (!currentFingerprint || currentFingerprint === spawnedFingerprint) return false;

    this._retiringDueToBinaryChange = true;
    this.logVia(
      `Binary contents changed on disk for ${this.binaryPath}; retiring this bridge so the next tool call respawns onto the updated binary.`,
    );
    return true;
  }

  /** Project root this bridge was spawned/configured for. */
  getCwd(): string {
    return this.cwd;
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

  /** Capture the host registration fact once without restarting a bridge created before registration. */
  setEditSlotSurvives(value: boolean): void {
    if (this.editSlotSurvivesCaptured) {
      throw new Error(
        `${this.errorPrefix} edit_slot_survives is write-once and was already captured`,
      );
    }
    this.editSlotSurvives = value;
    this.editSlotSurvivesCaptured = true;
  }

  private logHashlineRegistrationCarrier(
    phase: "configure" | "tool_call",
    sessionId: string | undefined,
    editSlotSurvives: boolean,
  ): void {
    const session = sessionId && sessionId.length > 0 ? sessionId : "__default__";
    const key = `${phase}\u0000${session}\u0000${String(editSlotSurvives)}`;
    const now = Date.now();
    const state = this.hashlineRegistrationLogState.get(key);
    if (state && now - state.lastEmittedAt < HASHLINE_REGISTRATION_LOG_INTERVAL_MS) {
      state.suppressed += 1;
      return;
    }
    const repeated = state && state.suppressed > 0 ? ` repeated=${state.suppressed + 1}` : "";
    if (!state && this.hashlineRegistrationLogState.size >= HASHLINE_REGISTRATION_LOG_STATE_LIMIT) {
      const oldest = this.hashlineRegistrationLogState.keys().next().value;
      if (oldest !== undefined) this.hashlineRegistrationLogState.delete(oldest);
    }
    this.hashlineRegistrationLogState.set(key, { lastEmittedAt: now, suppressed: 0 });
    this.sessionLogVia(
      sessionId,
      `hashline registration carrier transport=ndjson phase=${phase} edit_slot_survives=${editSlotSurvives}${repeated}`,
    );
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
    let dispatchParams = params;
    if (command === "configure") {
      dispatchParams = { ...params };
      delete dispatchParams.edit_slot_survives;
      const editSlotSurvives = this.editSlotSurvives;
      if (this.editSlotSurvivesCaptured && typeof editSlotSurvives === "boolean") {
        dispatchParams.edit_slot_survives = editSlotSurvives;
        const sessionId =
          typeof dispatchParams.session_id === "string" ? dispatchParams.session_id : undefined;
        this.logHashlineRegistrationCarrier("configure", sessionId, editSlotSurvives);
      }
    }
    return this.sendWithVersionMismatchRetry(command, dispatchParams, options, true);
  }

  /**
   * Dispatch an agent tool through the server-side `tool_call` command.
   *
   * The Rust command returns the direct leaf response plus one `text` field;
   * status_bar/bg_completions and every other sidecar stay top-level so the
   * existing plugin ingest/capture path sees the same raw response shape.
   */
  async toolCall(
    sessionId: string | undefined,
    name: string,
    rawArgs: ToolCallArguments = {},
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    const params: Record<string, unknown> = { name, arguments: rawArgs };
    if (sessionId) params.session_id = sessionId;
    const editSlotSurvives = this.editSlotSurvives;
    if (this.editSlotSurvivesCaptured && typeof editSlotSurvives === "boolean") {
      params.edit_slot_survives = editSlotSurvives;
      this.logHashlineRegistrationCarrier("tool_call", sessionId, editSlotSurvives);
    }
    const { preview, ...sendOptions } = options ?? {};
    if (preview === true) params.preview = true;
    return (await this.send(
      "tool_call",
      params,
      Object.keys(sendOptions).length > 0 ? (sendOptions as SendOptions) : undefined,
    )) as ToolCallResult;
  }

  private async sendWithVersionMismatchRetry(
    command: string,
    params: Record<string, unknown>,
    options: SendOptions | undefined,
    canRetryAfterVersionSwap: boolean,
  ): Promise<Record<string, unknown>> {
    try {
      if (this._retiringDueToBinaryChange) {
        throw new Error(
          `${this.errorPrefix} Bridge is retiring after the on-disk binary changed; retry to respawn on the updated binary`,
        );
      }
      if (this._shuttingDown) {
        throw new BridgeTransportUnavailableError(
          `${this.errorPrefix} Bridge is shutting down, cannot send "${command}"`,
        );
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

      // Per-op timeout override: tool wrappers can pass longer budgets for
      // commands that legitimately need them (callers, trace_to, grep on big
      // repos). Defaults to the bridge-wide timeout otherwise.
      // Passive health-check polls (status) get a short budget so they fall
      // back to the cached snapshot fast instead of blocking the full 30s
      // behind legitimate work. An explicit shorter override still wins.
      const passive = isPassiveCommand(command);
      const resolvedTimeoutMs = options?.transportTimeoutMs ?? options?.timeoutMs ?? this.timeoutMs;
      const effectiveTimeoutMs = passive
        ? Math.min(resolvedTimeoutMs, PASSIVE_COMMAND_TIMEOUT_MS)
        : resolvedTimeoutMs;
      const implicitTransportOptions: SendOptions = {
        ...(options?.transportTimeoutMs !== undefined || options?.timeoutMs !== undefined
          ? { transportTimeoutMs: effectiveTimeoutMs }
          : {}),
        markConfiguredOnSuccess: false,
      };

      // Auto-configure project root + plugin config on first command, then check version.
      // configured is set AFTER success to prevent skipping configuration on failure (#18).
      // When multiple parallel calls arrive before configure completes, they all await
      // the same promise instead of each independently trying to configure.
      if (!this.configured) {
        if (command !== "configure" && command !== "version") {
          if (!this._configurePromise) {
            const configuringChild = this.process;
            const requireConfiguringChild = (): void => {
              if (this.process !== configuringChild) {
                throw new BridgeTransportUnavailableError(
                  `${this.errorPrefix} Bridge process changed during configure; retry on replacement`,
                );
              }
            };
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
                const configResult = await this.send(
                  "configure",
                  {
                    project_root: this.cwd,
                    ...this.configOverrides,
                    ...(sessionIdForConfigure ? { session_id: sessionIdForConfigure } : {}),
                  },
                  implicitTransportOptions,
                );
                requireConfiguringChild();
                if (configResult.success === false) {
                  throw new Error(
                    `${this.errorPrefix} Configure failed: ${configResult.message ?? "unknown error"}`,
                  );
                }
                // Large-repo warning is emitted by the Rust side via log::warn!
                // and relayed through stderr → plugin log. No need to re-log here
                // (doing so would just duplicate the same line in aft-plugin.log).
                await this.deliverConfigureWarnings(configResult, params, options);
                requireConfiguringChild();
                await this.checkVersion(implicitTransportOptions);
                requireConfiguringChild();
                // Re-check liveness after version check — checkVersion() swallows
                // errors as best-effort, so the bridge may have died without throwing.
                if (!this.isAlive()) {
                  throw new BridgeTransportUnavailableError(
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

      // Passive polls NEVER count toward hang escalation regardless of caller:
      // a queued status poll timing out means the bridge is BUSY, not hung, and
      // killing it would abort the user's in-flight request waiting on the same
      // work (issue #117). This is enforced bridge-side so no call site can
      // forget it.
      const keepBridgeOnTimeout = passive || options?.keepBridgeOnTimeout === true;
      let requestSentAt = Date.now();

      const child = this.process;
      const response = await new Promise<Record<string, unknown>>((resolve, reject) => {
        const timer = setTimeout(() => {
          const entry = this.pending.get(id);
          if (!entry) return;
          this.pending.delete(id);
          clearTimeout(entry.timer);

          if (keepBridgeOnTimeout) {
            const timeoutMsg = `Request "${command}" (id=${id}) timed out after ${effectiveTimeoutMs}ms`;
            if (requestSessionId) {
              this.sessionWarnVia(requestSessionId, timeoutMsg);
            } else {
              this.warnVia(timeoutMsg);
            }
            entry.reject(
              new BridgeTransportTimeoutError(
                command,
                effectiveTimeoutMs,
                `${this.errorPrefix} Request "${command}" (id=${id}) timed out after ${effectiveTimeoutMs}ms`,
              ),
            );
            return;
          }

          const childActiveSinceRequest = this.lastChildActivityAt > requestSentAt;
          const consecutiveTimeouts = this.consecutiveRequestTimeouts + 1;
          this.consecutiveRequestTimeouts = consecutiveTimeouts;
          const keepWarm = childActiveSinceRequest || consecutiveTimeouts < this.hangThreshold;
          const restartSuffix = keepWarm ? " — bridge kept warm" : " — restarting bridge";
          const timeoutMsg = `Request "${command}" (id=${id}) timed out after ${effectiveTimeoutMs}ms${restartSuffix}`;
          if (requestSessionId) {
            this.sessionWarnVia(requestSessionId, timeoutMsg);
          } else {
            this.warnVia(timeoutMsg);
          }

          if (keepWarm) {
            entry.reject(
              new Error(
                `${this.errorPrefix} request "${command}" timed out after ${effectiveTimeoutMs}ms (bridge busy/under load); bridge kept warm — retry`,
              ),
            );
            return;
          }

          entry.reject(
            new Error(
              `${this.errorPrefix} Request "${command}" (id=${id}) timed out after ${effectiveTimeoutMs}ms`,
            ),
          );
          this.handleTimeout(requestSessionId);
        }, effectiveTimeoutMs);

        this.pending.set(id, { resolve, reject, timer, onProgress: options?.onProgress, command });

        if (!child?.stdin?.writable) {
          this.pending.delete(id);
          clearTimeout(timer);
          const error = new BridgeTransportUnavailableError(
            `${this.errorPrefix} stdin not writable for command "${command}"`,
          );
          reject(error);
          if (this.process === child) this.invalidateTransportProcess(error);
          return;
        }

        requestSentAt = Date.now();
        const handleWriteFailure = (cause: unknown): void => {
          const writeError = cause instanceof Error ? cause : new Error(String(cause));
          const error = new BridgeTransportUnknownOutcomeError(
            `${this.errorPrefix} Failed to write to stdin: ${writeError.message}`,
            { cause: writeError },
          );
          const entry = this.pending.get(id);
          if (entry) {
            this.pending.delete(id);
            clearTimeout(entry.timer);
            entry.reject(error);
          }
          if (this.process === child) this.invalidateTransportProcess(error);
        };
        try {
          child.stdin.write(line, (err) => {
            if (err) handleWriteFailure(err);
          });
        } catch (err) {
          handleWriteFailure(err);
        }
      });

      if (
        command === "configure" &&
        response.success === true &&
        options?.markConfiguredOnSuccess !== false
      ) {
        this.configured = true;
      }

      return response;
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
    if (!this.onConfigureWarnings) return;
    const warnings = Array.isArray(configResult.warnings)
      ? (configResult.warnings as ConfigureWarning[])
      : [];
    const configDroppedKeys = coerceConfigureDroppedKeys(configResult.config_dropped_keys);
    if (warnings.length === 0 && configDroppedKeys.length === 0) return;

    const sessionId = typeof params.session_id === "string" ? params.session_id : undefined;
    const context: ConfigureWarningsContext = {
      projectRoot: this.cwd,
      sessionId,
      client:
        options?.configureWarningClient ??
        (sessionId ? this.configureWarningClients.get(sessionId) : undefined),
      warnings,
    };
    if (configDroppedKeys.length > 0) {
      context.configDroppedKeys = configDroppedKeys;
    }
    try {
      await this.onConfigureWarnings(context);
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
    // Status-changed frames arrive frequently (every Tier-2 completion,
    // semantic progress tick, watcher refresh). Logging each one floods the
    // plugin log, so this cache update is intentionally silent.
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
    this.spawnedBinaryFingerprint = null;
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
  private async checkVersion(options?: SendOptions): Promise<void> {
    if (!this.minVersion) return;
    try {
      const resp = await this.send("version", {}, options);
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
    this.spawnedBinaryFingerprint = null;
    this.lastBinaryFingerprintCheckAt = 0;
    this._retiringDueToBinaryChange = false;
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
    if (this.isAlive() && this.process?.stdin?.writable) return;
    if (this.process !== null) {
      this.invalidateTransportProcess(
        new BridgeTransportUnavailableError(`${this.errorPrefix} Child stdin is not writable`),
      );
    }
    this.spawnProcess(triggeringSessionId);
  }

  private spawnProcess(triggeringSessionId?: string): void {
    this._retiringDueToBinaryChange = false;
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

    // Diagnostic: prove the spawnProcess code path executes and what
    // useFastembedBackend / parent ORT_DYLIB_PATH look like at spawn time.
    // The E2E harness asserts ORT_DYLIB_PATH propagation through plugin log;
    // earlier targeted log lines never appeared in CI runs even though the
    // dist contained them, so this unconditional marker proves whether the
    // code path is reached at all.
    this.logVia(
      `bridge.spawnProcess: useFastembedBackend=${useFastembedBackend}, ` +
        `parentORT=${process.env.ORT_DYLIB_PATH ?? "(unset)"}, ` +
        `ortLibraryPath=${ortLibraryPath ?? "(none)"}`,
    );
    if (useFastembedBackend) {
      // Store fastembed model files alongside the semantic index, not the project cwd.
      // This is only relevant when the fastembed backend is selected.
      env.FASTEMBED_CACHE_DIR =
        process.env.FASTEMBED_CACHE_DIR ||
        (typeof this.configOverrides.storage_dir === "string"
          ? join(this.configOverrides.storage_dir, "semantic", "models")
          : join(homedir() || "", ".cache", "fastembed"));

      // Point ort to the auto-downloaded or system ONNX Runtime library.
      // An explicit ORT_DYLIB_PATH in the parent environment wins — that
      // lets users (and the Docker/macOS E2E harnesses) test what happens
      // when ort can't load the library, without our managed-install
      // resolution silently masking the bad path. Log either way so the
      // E2E harness can assert the env var made it through.
      if (process.env.ORT_DYLIB_PATH) {
        this.logVia(`ORT_DYLIB_PATH inherited from parent env: ${process.env.ORT_DYLIB_PATH}`);
      } else if (ortLibraryPath) {
        env.ORT_DYLIB_PATH = ortLibraryPath;
        this.logVia(`ORT_DYLIB_PATH set from managed ONNX Runtime: ${ortLibraryPath}`);
      }
    }

    // Per-bridge child env overrides (e.g. AFT_CACHE_DIR in tests). Applied last
    // so they win over inherited/derived values, and scoped to THIS child only —
    // no shared process.env mutation, so concurrent bridges can't race.
    if (this.childEnv) {
      for (const [key, value] of Object.entries(this.childEnv)) {
        if (value === undefined) {
          delete env[key];
        } else {
          env[key] = value;
        }
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
      if (this.process !== currentChild) return;
      this.onStdoutData(stdoutDecoder.write(chunk));
    });
    child.stdout?.on("end", () => {
      if (this.process !== currentChild) return;
      const remaining = stdoutDecoder.end();
      if (remaining) this.onStdoutData(remaining);
      this.flushStdoutBuffer();
    });

    const stderrDecoder = new StringDecoder("utf8");
    child.stderr?.on("data", (chunk: Buffer) => {
      if (this.process !== currentChild) return;
      this.onStderrData(stderrDecoder.write(chunk));
    });
    child.stderr?.on("end", () => {
      if (this.process !== currentChild) return;
      const remaining = stderrDecoder.end();
      if (remaining) this.onStderrData(remaining);
      this.flushStderrBuffer();
    });

    child.on("error", (err) => {
      if (this.process !== currentChild) return;
      this.errorVia(`Process error: ${err.message}${this.formatStderrTail()}`);
      this.handleCrash(err);
    });

    child.on("exit", (code, signal) => {
      if (this.process !== currentChild) return;
      if (this._shuttingDown) return;
      this.flushStdoutBuffer();
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
        this.rejectAllPending(
          new BridgeTransportUnknownOutcomeError(`${this.errorPrefix} Binary killed by ${signal}`),
        );
        return;
      }
      this.handleCrash();
    });

    this.process = child;
    this.spawnedBinaryFingerprint = readBinaryFingerprint(this.binaryPath);
    this.lastBinaryFingerprintCheckAt = Date.now();
    this.stdoutBuffer = "";
    this.stdoutReadOffset = 0;
    this.stderrBuffer = "";
    this.lastChildActivityAt = 0;
    this.consecutiveRequestTimeouts = 0;
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
      if (!line || !shouldSurfaceStderrLine(line)) continue;
      const tagged = tagStderrLine(line);
      this.logVia(tagged);
      this.pushStderrLine(tagged);
    }
  }

  private flushStderrBuffer(): void {
    const line = this.stderrBuffer.replace(/\r$/, "");
    this.stderrBuffer = "";
    if (!line || !shouldSurfaceStderrLine(line)) return;
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
    if (this.stdoutReadOffset > STDOUT_BUFFER_COMPACT_THRESHOLD) {
      this.compactStdoutBuffer();
    }
    this.stdoutBuffer += data;
    if (this.stdoutBuffer.length - this.stdoutReadOffset > MAX_STDOUT_BUFFER) {
      this.handleCrash(
        new Error(`aft bridge stdout buffer exceeded ${MAX_STDOUT_BUFFER} bytes — killing bridge`),
      );
      return;
    }

    // Process complete lines without repeatedly slicing the remaining buffer.
    let newlineIdx: number;
    while ((newlineIdx = this.stdoutBuffer.indexOf("\n", this.stdoutReadOffset)) !== -1) {
      const line = this.stdoutBuffer.slice(this.stdoutReadOffset, newlineIdx).trim();
      this.stdoutReadOffset = newlineIdx + 1;

      if (line) {
        this.processStdoutLine(line);
      }

      if (
        this.stdoutReadOffset > STDOUT_BUFFER_COMPACT_THRESHOLD &&
        this.stdoutReadOffset > this.stdoutBuffer.length / 2
      ) {
        this.compactStdoutBuffer();
      }
    }

    if (this.stdoutReadOffset === this.stdoutBuffer.length) {
      this.stdoutBuffer = "";
      this.stdoutReadOffset = 0;
    }
  }

  private compactStdoutBuffer(): void {
    if (this.stdoutReadOffset === 0) return;
    this.stdoutBuffer = this.stdoutBuffer.slice(this.stdoutReadOffset);
    this.stdoutReadOffset = 0;
  }

  private flushStdoutBuffer(): void {
    const line = this.stdoutBuffer.slice(this.stdoutReadOffset).trim();
    this.stdoutBuffer = "";
    this.stdoutReadOffset = 0;
    if (!line) return;
    this.processStdoutLine(line);
  }

  private processStdoutLine(line: string): void {
    try {
      const response = JSON.parse(line) as Record<string, unknown>;
      this.lastChildActivityAt = Date.now();
      if (response.type === "progress") {
        const requestId = response.request_id as string | undefined;
        const entry = requestId ? this.pending.get(requestId) : undefined;
        const kind = response.kind === "stderr" ? "stderr" : "stdout";
        const text = typeof response.chunk === "string" ? response.chunk : "";
        entry?.onProgress?.({ kind, text });
        return;
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
        return;
      }
      if (response.type === "bash_completed") {
        const taskId = bashTaskIdFrom(response);
        if (taskId) this.outstandingBackgroundTaskIds.delete(taskId);
        this.onBashCompletion?.(response as unknown as BashCompletedPayload, this);
        return;
      }
      if (response.type === "bash_long_running") {
        this.onBashLongRunning?.(response as unknown as BashLongRunningPayload, this);
        return;
      }
      if (response.type === "bash_pattern_match") {
        this.onBashPatternMatch?.(response as unknown as BashPatternMatchFrame, this);
        return;
      }
      if (response.type === "configure_warnings") {
        this.handleConfigureWarningsFrame(response).catch((err) => {
          this.warnVia(
            `configure warning delivery failed: ${err instanceof Error ? err.message : String(err)}`,
          );
        });
        return;
      }
      if (response.type === "status_changed") {
        this.handleStatusChangedFrame(response);
        return;
      }
      const id = response.id as string | undefined;
      if (id && this.pending.has(id)) {
        const entry = this.pending.get(id);
        if (!entry) return;
        this.pending.delete(id);
        clearTimeout(entry.timer);
        this.consecutiveRequestTimeouts = 0;
        this.scheduleRestartCountReset();
        this.accountForBashTaskResponse(entry.command, response);
        entry.resolve(response);
      } else if (typeof response.type === "string") {
        this.logVia(`Ignoring unknown stdout push frame type: ${response.type}`);
      }
    } catch (_err) {
      this.warnVia(`Failed to parse stdout line: ${line}`);
    }
  }

  private accountForBashTaskResponse(command: string, response: Record<string, unknown>): void {
    const taskId = bashTaskIdFrom(response);
    if (!taskId) return;

    if (isTerminalBashStatus(response.status)) {
      this.outstandingBackgroundTaskIds.delete(taskId);
      return;
    }

    // A successful bash spawn returns { task_id, status: "running", ... }.
    // Bias toward wake safety: if a bash response has a task id but an unknown
    // non-terminal/missing status, keep the bridge alive until a terminal
    // bash_completed frame or terminal bash_status/bash_kill response removes it.
    if (command === "bash" && response.success !== false) {
      this.outstandingBackgroundTaskIds.add(taskId);
    }
  }

  private handleTimeout(triggeringSessionId?: string): void {
    this.consecutiveRequestTimeouts = 0;
    this.spawnedBinaryFingerprint = null;
    // A timed-out request means the child is about to be SIGKILLed. Reject all
    // sibling in-flight requests now instead of leaving them parked until their
    // own independent timers fire.
    this.rejectAllPending(
      new BridgeTransportUnknownOutcomeError(
        `${this.errorPrefix} bridge killed during sibling timeout — request aborted`,
      ),
    );
    // Forget outstanding background task ids: their removal hooks died with
    // the child (foreground polls were just rejected and won't resume, and a
    // completion frame can't arrive from a killed process), so keeping them
    // would pin this bridge against idle eviction forever. Safe to forget —
    // detached tasks persist undelivered completions on disk, and the next
    // spawn rehydrates and delivers them.
    this.outstandingBackgroundTaskIds.clear();
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
    this.spawnedBinaryFingerprint = null;
    if (proc && proc.exitCode === null && !proc.killed) {
      proc.kill("SIGKILL");
    }
    this.clearRestartResetTimer();
    this.configured = false; // Force reconfigure on next command after restart
    // Forget outstanding background task ids — same rationale as
    // handleTimeout: abandoned removal hooks would otherwise pin this bridge
    // against idle eviction permanently (phantom ids). Disk-persisted
    // completions are re-delivered after the next spawn rehydrates.
    this.outstandingBackgroundTaskIds.clear();

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
      new BridgeTransportUnknownOutcomeError(
        `${this.errorPrefix} Binary crashed (restarts: ${this._restartCount})${cause ? `: ${cause.message}` : ""} (see ${this.getLogFilePathVia()})`,
        { cause },
      ),
    );

    if (this._retiringDueToBinaryChange) {
      this.logVia("Binary exited while retiring after an on-disk update; skipping auto-restart");
      return;
    }

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
