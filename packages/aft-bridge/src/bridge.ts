import { homedir } from "node:os";
import { join } from "node:path";
import { StringDecoder } from "node:string_decoder";

import { error, getActiveLogger, getLogFilePath, log, warn } from "./active-logger.js";
import { binaryContentHash, binaryStampKey, peekBinaryContentHash } from "./binary-identity.js";
import { type ChildProcess, spawn } from "./child-process.js";
import {
  isPassiveCommand,
  isReadOnlyRetryCommand,
  PASSIVE_COMMAND_TIMEOUT_MS,
} from "./command-timeouts.js";
import type { Logger, LogMeta } from "./logger.js";
import { withPathPrepended } from "./path-env.js";
import { type BgCompletion, isTerminalBashStatus, type StatusCompression } from "./protocol.js";
import type {
  AftProjectTransport,
  ToolCallArguments,
  ToolCallOptions,
  ToolCallResult,
} from "./transport.js";
import { HOST_TOOL_NAMES_FIELD, WORKER_SESSION_FIELD } from "./transport.js";

const DEFAULT_BRIDGE_TIMEOUT_MS = 30_000;
/** Default floor of the hang watchdog's silence window (`hangWindowMinMs`). */
const DEFAULT_MIN_HANG_WINDOW_MS = 60_000;
/** Default wait for the answer to the hang watchdog's `ping` (`hangProbeTimeoutMs`). */
const DEFAULT_HANG_PROBE_TIMEOUT_MS = 5_000;

/**
 * How long the child may write nothing at all (no reply, progress frame or
 * push frame) while it owes a reply, before the hang watchdog probes it with a
 * `ping`. The window starts at the child's last output, or at the oldest
 * unanswered request when that request was sent after the last output.
 *
 * The window is twice the longest transport budget among those unanswered
 * requests and the calls still waiting, and never less than `minWindowMs`. A
 * request that is slow but healthy finishes within its own budget, and a
 * request queued behind it within roughly one more, so a child that stays
 * silent for twice the longest budget is suspected hung and gets the probe.
 * (A deeper silent backlog can still reach the window; any parsed stdout frame
 * resets it.) The floor keeps short budgets (5 s status polls, 30 s defaults)
 * from turning a slow disk or a cold index into a kill. This is the only place
 * the window is computed.
 */
export function hangWindowMs(
  budgetsMs: Iterable<number>,
  minWindowMs: number = DEFAULT_MIN_HANG_WINDOW_MS,
): number {
  let longest = 0;
  for (const budget of budgetsMs) longest = Math.max(longest, budget);
  return Math.max(minWindowMs, 2 * longest);
}
const MAX_STDOUT_BUFFER = 64 * 1024 * 1024; // 64MB
/** Characters of an unparseable stdout line quoted in the warning log. */
const MALFORMED_STDOUT_LOG_CHARS = 512;
const HASHLINE_REGISTRATION_LOG_INTERVAL_MS = 60_000;
const HASHLINE_REGISTRATION_LOG_STATE_LIMIT = 256;
function bashTaskIdFrom(response: Record<string, unknown>): string | undefined {
  const snakeCase = response.task_id;
  if (typeof snakeCase === "string" && snakeCase.length > 0) return snakeCase;
  const camelCase = response.taskId;
  if (typeof camelCase === "string" && camelCase.length > 0) return camelCase;
  return undefined;
}

type BinaryFingerprintReader = (binaryPath: string) => string | null | undefined;

/**
 * The bridge fingerprints its binary's bytes at spawn and compares that with
 * the file on disk during idle maintenance, so a bridge whose binary was
 * replaced (an upgrade or a local rebuild) retires and the next call respawns
 * onto the new binary. Hashing the ~85 MB binary synchronously stalled the
 * host thread on every spawn and every check, so the default reader never
 * reads the file itself: it answers from the identity sidecar or an earlier
 * hash of the same stamp, and otherwise starts a streamed hash and reports
 * "unknown for now".
 */
function peekBinaryFingerprint(binaryPath: string): string | null {
  return peekBinaryContentHash(binaryPath);
}

let binaryFingerprintReader: BinaryFingerprintReader = peekBinaryFingerprint;
/** Off-thread fingerprint used when the spawn-time peek had no answer yet. */
let binaryFingerprintAsyncReader: (
  binaryPath: string,
  expectedStampKey: string,
) => Promise<string | null> = binaryContentHash;

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
  binaryFingerprintReader = impl ?? peekBinaryFingerprint;
  binaryFingerprintAsyncReader = impl
    ? async (binaryPath) => {
        const fingerprint = impl(binaryPath);
        return typeof fingerprint === "string" && fingerprint.length > 0 ? fingerprint : null;
      }
    : binaryContentHash;
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
  const parse = (version: string): { core: string[]; pre: string[] } => {
    const match =
      /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/.exec(
        version,
      );
    const pre = match?.[4]?.split(".") ?? [];
    if (!match || pre.some((id) => /^\d+$/.test(id) && id.length > 1 && id[0] === "0")) {
      throw new Error(`Invalid semantic version: ${version}`);
    }
    return { core: match.slice(1, 4), pre };
  };
  // Compare numeric identifiers as strings so even large valid identifiers
  // retain their order without Number rounding or overflow.
  const numeric = (a: string, b: string): number =>
    a.length !== b.length ? a.length - b.length : a === b ? 0 : a < b ? -1 : 1;
  const left = parse(a);
  const right = parse(b);
  for (let i = 0; i < 3; i++) {
    const diff = numeric(left.core[i], right.core[i]);
    if (diff !== 0) return diff;
  }
  if (left.pre.length === 0 || right.pre.length === 0) {
    return left.pre.length === right.pre.length ? 0 : left.pre.length === 0 ? 1 : -1;
  }
  for (let i = 0; i < Math.max(left.pre.length, right.pre.length); i++) {
    const ai = left.pre[i];
    const bi = right.pre[i];
    if (ai === undefined) return -1;
    if (bi === undefined) return 1;
    const aNum = /^\d+$/.test(ai);
    const bNum = /^\d+$/.test(bi);
    const diff =
      aNum && bNum
        ? numeric(ai, bi)
        : aNum !== bNum
          ? aNum
            ? -1
            : 1
          : ai === bi
            ? 0
            : ai < bi
              ? -1
              : 1;
    if (diff !== 0) return diff;
  }
  return 0;
}

/** Opaque handle returned by {@link BridgeClock.setTimeout}. */
export type BridgeTimer = unknown;

/**
 * Time source for request deadlines, the bash late-reply grace and the hang
 * watchdog. Production uses the system clock; tests inject a manual clock so
 * deadlines and the watchdog fire exactly when the test advances time,
 * independent of machine load.
 */
export interface BridgeClock {
  now(): number;
  setTimeout(callback: () => void, ms: number): BridgeTimer;
  clearTimeout(timer: BridgeTimer): void;
}

// Looks up the global timer functions at call time, so code that replaces
// globalThis.setTimeout (as some tests do) still sees bridge timers.
const SYSTEM_CLOCK: BridgeClock = {
  now: () => Date.now(),
  setTimeout: (callback, ms) => setTimeout(callback, ms),
  clearTimeout: (timer) => clearTimeout(timer as ReturnType<typeof setTimeout>),
};

interface PendingRequest {
  resolve: (value: Record<string, unknown>) => void;
  reject: (error: Error) => void;
  timer: BridgeTimer;
  /** Transport budget of this call; widens the hang watchdog's window while it waits. */
  budgetMs: number;
  onProgress?: (chunk: { kind: "stdout" | "stderr"; text: string }) => void;
  onSettled?: () => void;
  command: string;
  tool: string;
  generation: number;
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
  /**
   * For a `bash`/`powershell` call, what the bridge learned about the command
   * while it waited a short grace period after the deadline. Undefined for
   * other commands.
   */
  readonly shellOutcome: ShellTimeoutOutcome | undefined;
  constructor(
    public readonly command: string,
    public readonly timeoutMs: number,
    message: string,
    shellOutcome?: ShellTimeoutOutcome,
  ) {
    super(message);
    this.name = "BridgeTransportTimeoutError";
    this.shellOutcome = shellOutcome;
  }
}

/**
 * What is known about a shell command whose call timed out at the bridge.
 *
 * - `task_started`: AFT answered after the deadline with this task id, so the
 *   command exists as a task the agent can poll.
 * - `not_started`: AFT answered after the deadline with an error and no task
 *   id, so it reported that it did not start the command.
 * - `unknown`: no usable answer arrived in the grace period, so the bridge
 *   cannot tell whether the command started.
 */
export type ShellTimeoutOutcome =
  | { kind: "task_started"; taskId: string }
  | { kind: "not_started"; code?: string; message?: string }
  | { kind: "unknown" };

/** Commands that start a shell task in AFT and answer with its task id. */
const SHELL_TASK_COMMANDS: ReadonlySet<string> = new Set(["bash", "powershell"]);

/**
 * How long a timed-out shell call keeps listening for AFT's late reply. AFT
 * can start the command and answer just after the bridge's deadline (for
 * example while it is busy with other work); that reply carries the task id,
 * which is the only way the agent can find the running command. Kept short
 * because the agent waits this much longer for its error.
 */
const DEFAULT_LATE_SHELL_REPLY_GRACE_MS = 10_000;

function shellOutcomeFromLateReply(response: Record<string, unknown>): ShellTimeoutOutcome {
  const taskId = bashTaskIdFrom(response);
  if (taskId) return { kind: "task_started", taskId };
  if (response.success === false) {
    return {
      kind: "not_started",
      ...(typeof response.code === "string" ? { code: response.code } : {}),
      ...(typeof response.message === "string" ? { message: response.message } : {}),
    };
  }
  // A successful reply without a task id is not a shape AFT sends for a shell
  // command, so it proves nothing either way.
  return { kind: "unknown" };
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

/**
 * The hang watchdog killed the child while this request was waiting: the child
 * had written nothing for longer than its silence window (see `hangWindowMs`).
 * The bridge respawns on the next call, so the request can be sent again.
 *
 * `outcomeUnknown` is false for read-only tools (`READ_ONLY_RETRY_COMMANDS`):
 * they cannot have changed anything, and the bridge retries them once on the
 * respawned child (unless the call was aborted or the bridge is shutting
 * down), so a caller sees this only if that retry was cut off too. For every
 * other command it is true: the child may
 * have applied the request before it went silent, so a mutation must be
 * inspected before it is repeated.
 */
export class BridgeRestartedError extends BridgeTransportUnknownOutcomeError {
  readonly retryable = true as const;
  readonly restartReason = "hang_watchdog" as const;
  constructor(
    public readonly tool: string,
    public readonly outcomeUnknown: boolean,
    message: string,
  ) {
    super(message);
    this.name = "BridgeRestartedError";
  }
}

/** Type guard for a rejection caused by a hang-watchdog restart. */
export function isBridgeRestarted(err: unknown): err is BridgeRestartedError {
  return err instanceof BridgeRestartedError;
}

/** Type guard for a transport-timeout rejection (bridge busy, retryable). */
export function isBridgeTransportTimeout(err: unknown): err is BridgeTransportTimeoutError {
  return err instanceof Error && (err as { code?: unknown }).code === "transport_timeout";
}

export interface BridgeOptions {
  /** Request timeout in milliseconds. Default: 30000 */
  timeoutMs?: number;
  /**
   * Floor of the hang watchdog's silence window in milliseconds. Default:
   * 60000. Tests lower it to exercise the watchdog quickly.
   */
  hangWindowMinMs?: number;
  /**
   * How long the hang watchdog waits for the answer to the `ping` it sends
   * once the child has been silent for its window. The child is killed only
   * when neither that answer nor any other stdout frame arrives in this time.
   * Default: 5000. Tests lower it to fit their timelines.
   */
  hangProbeTimeoutMs?: number;
  /**
   * Time source for request deadlines, the bash late-reply grace and the hang
   * watchdog. Default: the system clock. Tests inject a manual clock.
   */
  clock?: BridgeClock;
  /**
   * How long a `bash`/`powershell` call that timed out with
   * `keepBridgeOnTimeout` keeps waiting for AFT's late reply, so its timeout
   * error can name the task AFT started. 0 rejects at the deadline. Default:
   * 10000.
   */
  lateShellReplyGraceMs?: number;
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

export interface BashWatchLineBatch {
  retry_gap?: {
    reason: "retry_overflow";
    dropped_notifications: number;
    known_missing_occurrences: number;
    first_offset?: number;
    last_offset?: number;
    stream?: string;
    evicted_through?: number;
    eviction_epoch?: string;
    last_match_version?: number;
  };
  acknowledged_version?: number;
  pending_id?: string;
  version?: number;
  terminal_summary?: boolean;
  snapshot?: boolean;
  lines: {
    stream: string;
    offset: number;
    end_offset: number;
    text: string;
    count: number;
    unavailable?: boolean;
  }[];
  masking_limit_bytes?: number;
  dropped_lines: number;
  first_dropped_offset: number | null;
  first_dropped_stream: string | null;
  skipped_long_lines: number;
}

export interface BashPatternMatchFrame {
  batch?: BashWatchLineBatch;
  type: "bash_pattern_match";
  task_id: string;
  session_id: string;
  watch_id: string;
  match_text: string;
  match_offset: number;
  context: string;
  once: boolean;
  reason?: "pattern_match" | "task_exit";
}

/** Exact durable match identity accepted by bash_ack_completions.matches. */
export interface BashPatternMatchAck {
  task_id: string;
  watch_id: string;
  match_offset: number;
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
  /**
   * Host cancellation for one Rust request. Standalone sends `cancel_request`;
   * supervisor-backed subc closes the scoped route so the daemon cancels it.
   */
  abortSignal?: AbortSignal;
  /** Per-call transport timeout in milliseconds. Defaults to the bridge-wide timeout. */
  transportTimeoutMs?: number;
  /**
   * Reject a transport-level timeout of this request with a typed
   * `BridgeTransportTimeoutError` (code `transport_timeout`), and for
   * `bash`/`powershell` first wait a short grace period for AFT's late reply
   * naming the task it started. Without it the timeout rejects with a plain
   * error carrying the same outcome-unknown guidance.
   *
   * Either way a timeout rejects only this request: it never kills the child
   * or touches other pending requests. Restarting a hung child is the hang
   * watchdog's job, which judges the child by its silence (see
   * `hangWindowMs`), not by request timeouts. Passive polls and
   * `bash_drain_completions` always get this flag.
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
  /**
   * Public `send()` calls that have not settled yet, including the configure
   * and version requests a first call runs before its own request. While this
   * is above zero (or a background bash task is outstanding) the child process
   * and its pipes keep the host's event loop alive; otherwise they are
   * unreferenced, so an idle bridge never stops a one-shot host command from
   * exiting. See `syncEventLoopHold`.
   */
  private activeSendCalls = 0;
  /**
   * Whether the current child's handles are referenced. A fresh spawn starts
   * referenced, as Node and Bun create them. Tracked so `ref`/`unref` are
   * only called on a change: Bun counts them on its child pipes, so a
   * repeated `ref()` would need as many `unref()` calls to release the loop.
   */
  private processLoopReferenced = true;
  private outstandingBackgroundTaskIds = new Set<string>();
  private nextId = 1;
  private processGeneration = 0;
  /**
   * Chunks of the current stdout line that has not seen its newline yet. They
   * are joined once, when the newline arrives, so a response split across many
   * pipe chunks is copied and scanned once instead of once per chunk.
   */
  private stdoutPending: string[] = [];
  private stdoutPendingLength = 0;
  /** Characters examined for newlines; tests use it to prove the scan is linear. */
  private stdoutScannedChars = 0;

  /** Test hook: characters the stdout reader has searched for newlines. */
  __stdoutScannedCharsForTests(): number {
    return this.stdoutScannedChars;
  }
  private stderrBuffer = "";
  /** Ring buffer of the last N stderr lines, cleared on every spawn. */
  private stderrTail: string[] = [];
  private _restartCount = 0;
  private _shuttingDown = false;
  private timeoutMs: number;
  private hangWindowMinMs: number;
  private hangProbeTimeoutMs: number;
  private readonly clock: BridgeClock;
  private lateShellReplyGraceMs: number;
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
  private restartTimer: ReturnType<typeof setTimeout> | null = null;
  private restartResetTimer: ReturnType<typeof setTimeout> | null = null;
  /**
   * Hang evidence: requests written to the current child that it has not
   * answered. An entry stays after its caller timed out, because the child
   * still owes the reply. An entry is removed when the child answers that
   * request (`settleHangEvidence`), when an answered liveness probe shows the
   * child is alive while the entry's caller is gone (`onHangProbeAnswered`),
   * or when a new child starts (`resetHangWatchdog`). Output for other
   * requests does not remove it: the child may have answered one session and
   * then frozen on another's request.
   */
  private awaitingReply = new Map<
    string,
    { sentAt: number; budgetMs: number; tool: string; sessionId: string | undefined }
  >();
  /**
   * Bridge-clock time of the current child's last parsed stdout frame of any
   * kind. The watchdog's silence is measured from here, so a child that keeps
   * writing is never judged silent.
   */
  private lastChildActivity = Number.NEGATIVE_INFINITY;
  /**
   * The watchdog's `ping` to the current child that has not been answered
   * yet; at most one is in flight. It is not a caller's request: it is never
   * retried, never counts as hang evidence, and its answer is handled only by
   * `onHangProbeAnswered`.
   */
  private hangProbe: { id: string; sentAt: number; timer: BridgeTimer } | null = null;
  private hangWatchdogTimer: BridgeTimer | null = null;
  /** Set once per child when a suppressed watchdog kill has been logged. */
  private hangSuppressionLogged = false;
  // One watchdog restart may recover a genuine hang. Restarting again before
  // any tool succeeds only repeats the same cold startup work, so the
  // watchdog stands down until then. This survives spawn/configure/version
  // and is re-armed by a successful tool response.
  private timeoutRestartAwaitingRecovery = false;
  private errorPrefix: string;
  private readonly logger: Logger | undefined;
  private readonly childEnv: Record<string, string | undefined> | undefined;
  private readonly platform: NodeJS.Platform;

  constructor(
    binaryPath: string,
    cwd: string,
    options?: BridgeOptions,
    configOverrides?: Record<string, unknown>,
    editSlotSurvives?: boolean,
    platform: NodeJS.Platform = process.platform,
  ) {
    this.binaryPath = binaryPath;
    this.cwd = cwd;
    this.timeoutMs = options?.timeoutMs ?? DEFAULT_BRIDGE_TIMEOUT_MS;
    this.hangWindowMinMs = options?.hangWindowMinMs ?? DEFAULT_MIN_HANG_WINDOW_MS;
    this.hangProbeTimeoutMs = options?.hangProbeTimeoutMs ?? DEFAULT_HANG_PROBE_TIMEOUT_MS;
    this.clock = options?.clock ?? SYSTEM_CLOCK;
    this.lateShellReplyGraceMs =
      options?.lateShellReplyGraceMs ?? DEFAULT_LATE_SHELL_REPLY_GRACE_MS;
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
    this.platform = platform;
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
   * Fingerprint the binary this child was spawned from, without reading it on
   * the host thread. When the fingerprint is not known yet, it is hashed in the
   * background and adopted only if the file still has the stamp it had at
   * spawn and this is still the same child, so the fingerprint always
   * describes the bytes the running child came from.
   */
  private recordSpawnedBinaryFingerprint(): void {
    this.spawnedBinaryFingerprint = readBinaryFingerprint(this.binaryPath);
    if (this.spawnedBinaryFingerprint) return;
    const spawnStampKey = binaryStampKey(this.binaryPath);
    if (spawnStampKey === null) return;
    const generation = this.processGeneration;
    void binaryFingerprintAsyncReader(this.binaryPath, spawnStampKey).then(
      (fingerprint) => {
        if (
          fingerprint &&
          this.processGeneration === generation &&
          this.spawnedBinaryFingerprint === null
        ) {
          this.spawnedBinaryFingerprint = fingerprint;
        }
      },
      () => {},
    );
  }

  /**
   * Idle-window maintenance hook: when the on-disk binary changed since this
   * child spawned, stop routing new work to this bridge so the pool can let it
   * drain and retire it. A fingerprint that is still being computed counts as
   * unchanged; the next check sees the result.
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
    this.activeSendCalls += 1;
    this.syncEventLoopHold();
    try {
      return await this.dispatchSend(command, params, options);
    } finally {
      this.activeSendCalls -= 1;
      this.syncEventLoopHold();
    }
  }

  /**
   * Reference the child process and its stdio pipes while the bridge has work
   * in flight, and unreference them while it is idle.
   *
   * A host that loads the plugin and then has nothing left to do (a one-shot
   * CLI command, or a headless run after its last turn) must be able to exit
   * even though a warm `aft` child is still running; the child sees its stdin
   * close and exits on its own. While a request is in flight the handles stay
   * referenced, so the host cannot exit mid-call and lose the response. A
   * background bash task the child is still running counts as work in flight
   * too: its completion frame can wake the session, so the host stays up for
   * it, as it always did before idle bridges were released.
   * `ref`/`unref` are optional because not every runtime exposes them on every
   * pipe (Bun's child stdin has neither and does not hold the loop).
   */
  private syncEventLoopHold(): void {
    const child = this.process;
    if (!child) return;
    const hold = this.activeSendCalls > 0 || this.outstandingBackgroundTaskIds.size > 0;
    if (hold === this.processLoopReferenced) return;
    this.processLoopReferenced = hold;
    // The stdio pipes are typed as plain streams, but at runtime they are
    // sockets (Node) or native readables (Bun) that carry ref/unref.
    const handles = [child, child.stdin, child.stdout, child.stderr] as unknown as Array<
      { ref?: () => unknown; unref?: () => unknown } | null | undefined
    >;
    for (const handle of handles) {
      if (hold) handle?.ref?.();
      else handle?.unref?.();
    }
  }

  private async dispatchSend(
    command: string,
    params: Record<string, unknown>,
    options: SendOptions | undefined,
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
  private listenForRequestAbort(requestId: string, signal: AbortSignal | undefined): () => void {
    if (!signal) return () => {};
    let fired = false;
    const onAbort = () => {
      if (fired) return;
      fired = true;
      void this.send("cancel_request", { id: requestId }, { keepBridgeOnTimeout: true }).catch(
        () => {
          // Transport teardown can win the race with best-effort cancellation.
        },
      );
    };
    signal.addEventListener("abort", onAbort, { once: true });
    if (signal.aborted) onAbort();
    return () => signal.removeEventListener("abort", onAbort);
  }

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
    const { preview, workerSession, hostToolNames, ...sendOptions } = options ?? {};
    if (preview === true) params.preview = true;
    if (workerSession === true) params[WORKER_SESSION_FIELD] = true;
    if (hostToolNames) params[HOST_TOOL_NAMES_FIELD] = hostToolNames;
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
    retriedAfterHangRestart = false,
  ): Promise<Record<string, unknown>> {
    try {
      if (this._retiringDueToBinaryChange) {
        throw new BridgeTransportUnavailableError(
          `${this.errorPrefix} Bridge is retiring after the on-disk binary changed; retry to respawn on the updated binary`,
        );
      }
      if (this._shuttingDown) {
        throw new BridgeTransportUnavailableError(
          `${this.errorPrefix} Bridge is shutting down, cannot send "${command}"`,
        );
      }

      if (Object.hasOwn(params, "id") && command !== "cancel_request") {
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
            // Only the initiating session owns configure's deferred delivery.
            // Later requests must not retain unrelated notification clients.
            if (
              this.onConfigureWarnings &&
              requestSessionId &&
              options?.configureWarningClient !== undefined
            ) {
              this.configureWarningClients.set(requestSessionId, options.configureWarningClient);
            }
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
              } catch (error) {
                if (sessionIdForConfigure)
                  this.configureWarningClients.delete(sessionIdForConfigure);
                throw error;
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
      const tool =
        command === "tool_call" && typeof params.name === "string" ? params.name : command;
      // Wire format: when params contains a key that collides with the protocol
      // envelope (`command`/`method`), nest params under a `params` key so the
      // outer dispatch dispatches on `command: "<bridge command>"` rather than
      // the user's payload key. Reserved envelope fields (`session_id`,
      // `lsp_hints`) must STILL be promoted to the top level so RawRequest's
      // dedicated fields deserialize correctly. Without this promotion, e.g.
      // `bash` (whose params include `command: "<shell command>"`) silently
      // loses `session_id` because it stays nested inside `params`.
      let request: Record<string, unknown>;
      if (
        command === "cancel_request" ||
        Object.hasOwn(params, "command") ||
        Object.hasOwn(params, "method")
      ) {
        const nested: Record<string, unknown> = { ...params };
        const reserved: Record<string, unknown> = {};
        for (const key of [
          "session_id",
          "lsp_hints",
          WORKER_SESSION_FIELD,
          HOST_TOOL_NAMES_FIELD,
        ] as const) {
          if (Object.hasOwn(nested, key)) {
            reserved[key] = nested[key];
            delete nested[key];
          }
        }
        request = { id, command, ...reserved, params: nested };
      } else {
        request = { id, command, ...params };
      }
      delete request.handles_progress;
      if (options?.onProgress) request.handles_progress = true;
      const line = `${JSON.stringify(request)}\n`;

      // Passive polls and completion drains always take the typed keep-warm
      // timeout regardless of caller: a queued status poll timing out means
      // the bridge is BUSY, not hung (issue #117). Enforced bridge-side so no
      // call site can forget it.
      const keepBridgeOnTimeout =
        passive || command === "bash_drain_completions" || options?.keepBridgeOnTimeout === true;

      const child = this.process;
      const response = await new Promise<Record<string, unknown>>((resolve, reject) => {
        // A timeout settles only this request. It never kills the child or
        // rejects other requests: one slow call says nothing about the calls
        // of other sessions sharing this child, and a child that is truly
        // hung is caught by the hang watchdog, which watches its silence.
        const timer = this.clock.setTimeout(() => {
          const entry = this.pending.get(id);
          if (!entry) return;
          if (
            keepBridgeOnTimeout &&
            this.holdForLateShellReply(id, entry, effectiveTimeoutMs, requestSessionId)
          ) {
            return;
          }
          this.pending.delete(id);
          this.clock.clearTimeout(entry.timer);
          entry.onSettled?.();

          const timeoutMsg = `Request "${command}" (id=${id}) timed out after ${effectiveTimeoutMs}ms — bridge kept warm`;
          if (requestSessionId) {
            this.sessionWarnVia(requestSessionId, timeoutMsg);
          } else {
            this.warnVia(timeoutMsg);
          }

          if (keepBridgeOnTimeout) {
            entry.reject(
              new BridgeTransportTimeoutError(
                command,
                effectiveTimeoutMs,
                `${this.errorPrefix} Request "${command}" (id=${id}) timed out after ${effectiveTimeoutMs}ms`,
              ),
            );
            return;
          }
          entry.reject(
            new Error(
              `${this.errorPrefix} request "${command}" timed out after ${effectiveTimeoutMs}ms (bridge busy/under load); bridge kept warm — retry. A timed-out mutation may still complete; inspect before repeating it.`,
            ),
          );
        }, effectiveTimeoutMs);

        const entry: PendingRequest = {
          resolve,
          reject,
          timer,
          budgetMs: effectiveTimeoutMs,
          onProgress: options?.onProgress,
          command,
          tool,
          generation: this.processGeneration,
        };
        this.pending.set(id, entry);

        if (!child?.stdin?.writable) {
          this.pending.delete(id);
          this.clock.clearTimeout(timer);
          const error = new BridgeTransportUnavailableError(
            `${this.errorPrefix} stdin not writable for command "${command}"`,
          );
          reject(error);
          if (this.process === child) this.invalidateTransportProcess(error);
          return;
        }

        this.recordAwaitingReply(id, effectiveTimeoutMs, tool, requestSessionId);
        const handleWriteFailure = (cause: unknown): void => {
          const writeError = cause instanceof Error ? cause : new Error(String(cause));
          const error = new BridgeTransportUnknownOutcomeError(
            `${this.errorPrefix} Failed to write to stdin: ${writeError.message}`,
            { cause: writeError },
          );
          const entry = this.pending.get(id);
          if (entry) {
            this.pending.delete(id);
            this.clock.clearTimeout(entry.timer);
            entry.onSettled?.();
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
        if (this.pending.has(id)) {
          entry.onSettled = this.listenForRequestAbort(id, options?.abortSignal);
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
        return this.sendWithVersionMismatchRetry(
          command,
          params,
          options,
          false,
          retriedAfterHangRestart,
        );
      }
      // A watchdog restart rejected this call. A read-only tool is sent once
      // more on the respawned child; anything else may already have taken
      // effect, so its caller gets the typed restart error to decide on.
      const tool =
        command === "tool_call" && typeof params.name === "string" ? params.name : command;
      if (
        err instanceof BridgeRestartedError &&
        !retriedAfterHangRestart &&
        isReadOnlyRetryCommand(tool) &&
        !this._shuttingDown &&
        options?.abortSignal?.aborted !== true
      ) {
        this.logVia(`Retrying read-only request "${tool}" once after the hang watchdog restart`);
        return this.sendWithVersionMismatchRetry(
          command,
          params,
          options,
          canRetryAfterVersionSwap,
          true,
        );
      }
      throw err;
    }
  }

  private async deliverConfigureWarnings(
    configResult: Record<string, unknown>,
    params: Record<string, unknown>,
    options: SendOptions | undefined,
  ): Promise<void> {
    const sessionId = typeof params.session_id === "string" ? params.session_id : undefined;
    const client =
      options?.configureWarningClient ??
      (sessionId ? this.configureWarningClients.get(sessionId) : undefined);
    // Capture ownership before dispatch. A hung host callback must neither hold
    // configure readiness nor delay releasing the terminal delivery's client.
    if (sessionId && configResult.warnings_pending !== true)
      this.configureWarningClients.delete(sessionId);
    if (!this.onConfigureWarnings) return;
    const warnings = Array.isArray(configResult.warnings)
      ? (configResult.warnings as ConfigureWarning[])
      : [];
    const configDroppedKeys = coerceConfigureDroppedKeys(configResult.config_dropped_keys);
    if (warnings.length === 0 && configDroppedKeys.length === 0) return;
    const context: ConfigureWarningsContext = {
      projectRoot: this.cwd,
      sessionId,
      client,
      warnings,
    };
    if (configDroppedKeys.length > 0) context.configDroppedKeys = configDroppedKeys;
    this.dispatchHostCallback("configure warning", () => this.onConfigureWarnings?.(context));
  }

  /** Release deferred-warning ownership even when the terminal frame has no warnings. */
  private async handleConfigureWarningsFrame(frame: Record<string, unknown>): Promise<void> {
    const sessionId =
      typeof frame.session_id === "string" && frame.session_id.length > 0 ? frame.session_id : null;
    const client = sessionId ? this.configureWarningClients.get(sessionId) : undefined;
    if (sessionId) this.configureWarningClients.delete(sessionId);
    if (!this.onConfigureWarnings || !Array.isArray(frame.warnings) || frame.warnings.length === 0)
      return;
    this.dispatchHostCallback("configure warning", () =>
      this.onConfigureWarnings?.({
        projectRoot: typeof frame.project_root === "string" ? frame.project_root : this.cwd,
        sessionId,
        client,
        warnings: frame.warnings as ConfigureWarning[],
      }),
    );
  }

  private dispatchHostCallback(label: string, callback: () => void | Promise<void>): void {
    const failed = (error: unknown): void => {
      this.warnVia(
        `${label} delivery failed: ${error instanceof Error ? error.message : String(error)}`,
      );
    };
    try {
      void Promise.resolve(callback()).catch(failed);
    } catch (error) {
      failed(error);
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
    this.resetHangWatchdog();
    this.clearRestartResetTimer();
    this.clearRestartTimer();
    this.configureWarningClients.clear();
    this.rejectAllPending(
      new BridgeTransportUnknownOutcomeError(`${this.errorPrefix} Bridge shutting down`),
    );

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
      new BridgeTransportUnknownOutcomeError(
        `${this.errorPrefix} Bridge restarting with updated binary: ${newBinaryPath}`,
      ),
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
    this.clearRestartTimer();
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
            this.platform === "win32"
              ? "onnxruntime.dll"
              : this.platform === "darwin"
                ? "libonnxruntime.dylib"
                : "libonnxruntime.so",
          );
    const env: NodeJS.ProcessEnv = { ...process.env };

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

    // Per-bridge child env overrides (e.g. AFT_CACHE_DIR in tests) are scoped to
    // THIS child only, so concurrent bridges never race through process.env. PATH
    // overrides supply the inherited tail; managed ONNX still stays at the front.
    if (this.childEnv) {
      for (const [key, value] of Object.entries(this.childEnv)) {
        if (this.platform === "win32" && key.toLowerCase() === "path") {
          for (const envKey of Object.keys(env)) {
            if (envKey.toLowerCase() === "path") delete env[envKey];
          }
        }
        if (value === undefined) {
          delete env[key];
        } else {
          env[key] = value;
        }
      }
    }

    const spawnEnv = withPathPrepended(
      env,
      this.platform === "win32" ? ortDir : undefined,
      this.platform,
    );
    const child = spawn(this.binaryPath, [], {
      cwd: this.cwd,
      stdio: ["pipe", "pipe", "pipe"],
      env: spawnEnv,
    });
    const currentChild = child;
    // Writable streams emit an error in addition to invoking write callbacks.
    // Keep retired children observed too, but only retire the current transport.
    child.stdin?.on("error", (cause) => {
      if (this.process !== currentChild) return;
      this.invalidateTransportProcess(
        new BridgeTransportUnknownOutcomeError(
          `${this.errorPrefix} Failed to write to stdin: ${cause.message}`,
          { cause },
        ),
      );
    });

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
    this.processGeneration += 1;
    this.recordSpawnedBinaryFingerprint();
    this.lastBinaryFingerprintCheckAt = Date.now();
    this.stdoutPending = [];
    this.stdoutPendingLength = 0;
    this.stderrBuffer = "";
    this.resetHangWatchdog();
    // Fresh spawn — clear the stderr ring so crash diagnostics only reflect
    // the current child's output, not output from prior restart cycles.
    this.stderrTail = [];
    // A child spawned by a call is held until that call settles; one spawned
    // by an auto-restart with nothing in flight is released right away.
    this.processLoopReferenced = true;
    this.syncEventLoopHold();
  }

  private pushStderrLine(line: string): void {
    this.stderrTail.push(line);
    if (this.stderrTail.length > BinaryBridge.STDERR_TAIL_MAX) {
      this.stderrTail.shift();
    }
  }

  private onStderrData(data: string): void {
    // Search each incoming segment once; never concatenate an unbounded partial record.
    const partialLimit = 64 * 1024;
    let offset = 0;
    while (offset < data.length) {
      const newline = data.indexOf("\n", offset);
      const end = newline < 0 ? data.length : newline;
      while (offset < end) {
        const count = Math.min(partialLimit - this.stderrBuffer.length, end - offset);
        this.stderrBuffer += data.slice(offset, offset + count);
        offset += count;
        if (this.stderrBuffer.length === partialLimit) {
          const last = this.stderrBuffer.charCodeAt(this.stderrBuffer.length - 1);
          const carry = last >= 0xd800 && last <= 0xdbff ? this.stderrBuffer.slice(-1) : "";
          const fragment = carry ? this.stderrBuffer.slice(0, -1) : this.stderrBuffer;
          const tagged = tagStderrLine(
            `${fragment} [stderr record truncated into bounded fragments; ${Buffer.byteLength(fragment)} bytes shown, continuation follows]`,
          );
          this.stderrBuffer = carry;
          this.logVia(tagged);
          this.pushStderrLine(tagged);
        }
      }
      if (newline < 0) break;
      this.flushStderrBuffer();
      offset = newline + 1;
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
    const exceedsLimit = (fragment: string): boolean => {
      if (this.stdoutPendingLength + Buffer.byteLength(fragment, "utf8") <= MAX_STDOUT_BUFFER)
        return false;
      this.handleCrash(
        new Error(`aft bridge stdout buffer exceeded ${MAX_STDOUT_BUFFER} bytes — killing bridge`),
      );
      return true;
    };

    // Only the new chunk is searched for newlines: the carried chunks are
    // already known to contain none.
    this.stdoutScannedChars += data.length;
    const generation = this.processGeneration;
    let start = 0;
    let newlineIdx = data.indexOf("\n");
    while (newlineIdx !== -1) {
      let line = data.slice(start, newlineIdx);
      if (exceedsLimit(line)) return;
      if (this.stdoutPending.length > 0) {
        this.stdoutPending.push(line);
        line = this.stdoutPending.join("");
        this.stdoutPending = [];
        this.stdoutPendingLength = 0;
      }
      start = newlineIdx + 1;
      line = line.trim();
      if (line) {
        this.processStdoutLine(line);
        // A handler that replaced the child owns the buffer now; the rest of
        // this chunk belongs to the old child and must not leak into it.
        if (this.processGeneration !== generation) return;
      }
      newlineIdx = data.indexOf("\n", start);
    }

    if (start < data.length) {
      const rest = start === 0 ? data : data.slice(start);
      if (exceedsLimit(rest)) return;
      this.stdoutPending.push(rest);
      this.stdoutPendingLength += Buffer.byteLength(rest, "utf8");
    }
  }

  private flushStdoutBuffer(): void {
    const line = this.stdoutPending.join("").trim();
    this.stdoutPending = [];
    this.stdoutPendingLength = 0;
    if (!line) return;
    this.processStdoutLine(line);
  }

  private processStdoutLine(line: string): void {
    try {
      const response = JSON.parse(line) as Record<string, unknown>;
      // Any parsed frame proves the child is alive: it restarts the silence
      // clock but forgets no unanswered request (see `awaitingReply`).
      this.lastChildActivity = this.clock.now();
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
          this.clock.clearTimeout(entry.timer);
          entry.onSettled?.();
          entry.resolve({
            success: false,
            code: "permission_required",
            message: "bash command requires permission",
            asks: response.asks,
          });
        }
        // The ask settles the request in place of a reply. This runs after the
        // pending entry is gone, so its budget no longer widens the window.
        if (requestId) this.settleHangEvidence(requestId);
        return;
      }
      if (response.type === "bash_completed") {
        const taskId = bashTaskIdFrom(response);
        if (taskId) this.outstandingBackgroundTaskIds.delete(taskId);
        this.syncEventLoopHold();
        this.dispatchHostCallback("bash completion", () =>
          this.onBashCompletion?.(response as unknown as BashCompletedPayload, this),
        );
        return;
      }
      if (response.type === "bash_long_running") {
        this.dispatchHostCallback("bash long-running", () =>
          this.onBashLongRunning?.(response as unknown as BashLongRunningPayload, this),
        );
        return;
      }
      if (response.type === "bash_pattern_match") {
        this.dispatchHostCallback("bash pattern-match", () =>
          this.onBashPatternMatch?.(response as unknown as BashPatternMatchFrame, this),
        );
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
      if (id && this.hangProbe?.id === id) {
        this.onHangProbeAnswered();
        return;
      }
      if (id && this.pending.has(id)) {
        const entry = this.pending.get(id);
        if (!entry) return;
        this.pending.delete(id);
        // Answered: the child no longer owes this request. Settled after the
        // pending entry is gone, so its budget no longer widens the window.
        this.settleHangEvidence(id);
        this.clock.clearTimeout(entry.timer);
        entry.onSettled?.();
        if (
          response.success !== false &&
          ![
            "configure",
            "version",
            "status",
            "ping",
            "bash_drain_completions",
            "bash_ack_completions",
            "cancel_request",
          ].includes(entry.command)
        ) {
          this.timeoutRestartAwaitingRecovery = false;
        }
        this.scheduleRestartCountReset();
        this.accountForBashTaskResponse(entry.command, response);
        entry.resolve(response);
      } else if (id && typeof response.type !== "string") {
        // A late reply still means the child no longer owes this request.
        this.settleHangEvidence(id);
        // A reply for a request that already settled (its deadline and any
        // grace period passed). One that names a task is a warning: the task
        // may still be running, and this log line is the only record of its id.
        const taskId = bashTaskIdFrom(response);
        if (taskId) {
          this.warnVia(`Dropped late reply for settled request id=${id} task_id=${taskId}`);
        } else {
          this.logVia(`Dropped late reply for settled request id=${id}`);
        }
      } else if (typeof response.type === "string") {
        this.logVia(`Ignoring unknown stdout push frame type: ${response.type}`);
      }
    } catch (_err) {
      // A malformed frame can be megabytes long; quote only its start.
      const quoted =
        line.length > MALFORMED_STDOUT_LOG_CHARS
          ? `${line.slice(0, MALFORMED_STDOUT_LOG_CHARS)}… (${line.length} chars)`
          : line;
      this.warnVia(`Failed to parse stdout line: ${quoted}`);
    }
  }

  /**
   * Keep a timed-out shell call listening for AFT's reply for a short grace
   * period instead of rejecting it at once. AFT may already have started the
   * command; its late reply carries the task id, and dropping it would leave the
   * agent with a running task it cannot find, which it may then start twice.
   *
   * The entry stays in `pending`, so the ordinary reply path still records the
   * task (see accountForBashTaskResponse) and clears the grace timer. The call
   * then rejects with a timeout error whose `shellOutcome` says what AFT
   * answered, or that nothing arrived. Returns false when the call is not a
   * shell command or the grace period is disabled; the caller then rejects as
   * before.
   */
  private holdForLateShellReply(
    id: string,
    entry: PendingRequest,
    effectiveTimeoutMs: number,
    requestSessionId: string | undefined,
  ): boolean {
    const graceMs = this.lateShellReplyGraceMs;
    if (!SHELL_TASK_COMMANDS.has(entry.command) || graceMs <= 0) return false;

    const timeoutMsg = `Request "${entry.command}" (id=${id}) timed out after ${effectiveTimeoutMs}ms; waiting up to ${graceMs}ms for a late reply naming its task`;
    if (requestSessionId) {
      this.sessionWarnVia(requestSessionId, timeoutMsg);
    } else {
      this.warnVia(timeoutMsg);
    }

    const message = `${this.errorPrefix} Request "${entry.command}" (id=${id}) timed out after ${effectiveTimeoutMs}ms`;
    const rejectWith = entry.reject;
    const settle = (outcome: ShellTimeoutOutcome): void => {
      rejectWith(
        new BridgeTransportTimeoutError(entry.command, effectiveTimeoutMs, message, outcome),
      );
    };
    // A reply now arrives after the caller's deadline, so it settles the call
    // as a timeout that reports what AFT answered.
    entry.resolve = (response) => settle(shellOutcomeFromLateReply(response));
    this.clock.clearTimeout(entry.timer);
    entry.timer = this.clock.setTimeout(() => {
      if (this.pending.get(id) !== entry) return;
      this.pending.delete(id);
      entry.onSettled?.();
      this.warnVia(
        `Request "${entry.command}" (id=${id}) got no reply within ${graceMs}ms after its deadline`,
      );
      settle({ kind: "unknown" });
    }, graceMs);
    return true;
  }

  private accountForBashTaskResponse(command: string, response: Record<string, unknown>): void {
    const taskId = bashTaskIdFrom(response);
    if (!taskId) return;

    if (isTerminalBashStatus(response.status, true)) {
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

  /** Record a request the child now owes a reply, and make sure the watchdog runs. */
  private recordAwaitingReply(
    id: string,
    budgetMs: number,
    tool: string,
    sessionId: string | undefined,
  ): void {
    this.awaitingReply.set(id, { sentAt: this.clock.now(), budgetMs, tool, sessionId });
    this.armHangWatchdog();
  }

  /**
   * The child answered request `id`, so it no longer owes that reply. Only
   * this request's evidence goes: other unanswered requests stay, because a
   * child can answer one session and then freeze on another's request.
   */
  private settleHangEvidence(id: string): void {
    if (!this.awaitingReply.delete(id)) return;
    // The window may have shrunk with this request's budget, so the timer is
    // set again from the remaining evidence (or dropped when none is left).
    this.clearHangWatchdogTimer();
    this.armHangWatchdog();
  }

  /**
   * When the current child counts as silent for too long: the silence window
   * measured from its last output, or from the oldest unanswered request when
   * that was sent later (a child owes nothing before it is asked). The window
   * covers the budgets of the unanswered requests and of every call still
   * waiting, so a slow request that is still within its budget widens it.
   * Null when the child owes no reply.
   */
  private hangDeadline(): {
    deadline: number;
    windowMs: number;
    silentSince: number;
    oldestId: string;
  } | null {
    if (this.awaitingReply.size === 0) return null;
    let oldestId = "";
    let oldestSentAt = Number.POSITIVE_INFINITY;
    const budgets: number[] = [];
    for (const [id, entry] of this.awaitingReply) {
      budgets.push(entry.budgetMs);
      if (entry.sentAt < oldestSentAt) {
        oldestSentAt = entry.sentAt;
        oldestId = id;
      }
    }
    for (const entry of this.pending.values()) budgets.push(entry.budgetMs);
    const windowMs = hangWindowMs(budgets, this.hangWindowMinMs);
    const silentSince = Math.max(oldestSentAt, this.lastChildActivity);
    return { deadline: silentSince + windowMs, windowMs, silentSince, oldestId };
  }

  private armHangWatchdog(): void {
    // While a probe is in flight, its own timer decides what happens next.
    if (this.hangWatchdogTimer || this.hangProbe) return;
    const hang = this.hangDeadline();
    if (!hang) return;
    const timer = this.clock.setTimeout(
      () => {
        this.hangWatchdogTimer = null;
        this.checkForHang();
      },
      Math.max(0, hang.deadline - this.clock.now()),
    );
    // Waiting calls have their own referenced timeout timers that keep the
    // host alive; an idle host must be able to exit with a hung child.
    (timer as { unref?: () => void }).unref?.();
    this.hangWatchdogTimer = timer;
  }

  private clearHangWatchdogTimer(): void {
    if (this.hangWatchdogTimer) this.clock.clearTimeout(this.hangWatchdogTimer);
    this.hangWatchdogTimer = null;
  }

  private clearHangProbe(): void {
    if (this.hangProbe) this.clock.clearTimeout(this.hangProbe.timer);
    this.hangProbe = null;
  }

  private resetHangWatchdog(): void {
    this.clearHangWatchdogTimer();
    this.clearHangProbe();
    this.awaitingReply.clear();
    this.lastChildActivity = Number.NEGATIVE_INFINITY;
    this.hangSuppressionLogged = false;
  }

  /**
   * Runs when the silence window may have elapsed. A child that owes a reply
   * and has written nothing for the whole window is either hung, or healthy
   * and idle while one request it was given never finishes (a stuck worker, a
   * lost reply). Only a hung child should be replaced: the bridge is shared by
   * every session, and a restart throws away everyone's warm state. So the
   * watchdog sends a `ping` (answered by the engine's dispatcher without
   * touching any index) and decides when it is answered or times out; see
   * `onHangProbeAnswered` and `onHangProbeTimeout`.
   *
   * The probe goes out at this tick even when no caller is waiting any more
   * (every caller may already have timed out). Waiting for the next call
   * instead would let that call run into the same silent child. So a hung
   * child is replaced one window plus one probe timeout after its silence
   * began (its last output, or the oldest unanswered request if that came
   * later), and a call made in that time waits at most until then.
   *
   * After one watchdog restart it stands down until a tool succeeds (see
   * `timeoutRestartAwaitingRecovery`), and then sends no probe either.
   */
  private checkForHang(): void {
    if (!this.isAlive() || this.hangProbe) return;
    const hang = this.hangDeadline();
    if (!hang) return;
    const now = this.clock.now();
    if (now < hang.deadline) {
      this.armHangWatchdog();
      return;
    }
    if (this.timeoutRestartAwaitingRecovery) {
      // Pending calls are left to their own timeouts; see the field comment.
      if (!this.hangSuppressionLogged) {
        this.hangSuppressionLogged = true;
        this.warnVia(
          `Hang watchdog: child silent past its window, but it was already restarted once and no tool has succeeded since; not restarting again; ${this.hangAttribution(hang)}`,
        );
      }
      return;
    }
    this.sendHangProbe(hang);
  }

  private hangAttribution(hang: NonNullable<ReturnType<BinaryBridge["hangDeadline"]>>): string {
    return (
      `silent_ms=${this.clock.now() - hang.silentSince} ` +
      `hang_window_ms=${hang.windowMs} ` +
      `oldest_unanswered_request_id=${hang.oldestId} ` +
      `oldest_unanswered_tool=${JSON.stringify(this.awaitingReply.get(hang.oldestId)?.tool)} ` +
      `bridge_generation=${this.processGeneration}`
    );
  }

  /** Write the liveness `ping` straight to the child, outside the request path. */
  private sendHangProbe(hang: NonNullable<ReturnType<BinaryBridge["hangDeadline"]>>): void {
    const id = `hang-probe-${this.nextId++}`;
    const timer = this.clock.setTimeout(() => this.onHangProbeTimeout(id), this.hangProbeTimeoutMs);
    // Like the watchdog timer, the probe must not keep an idle host alive.
    (timer as { unref?: () => void }).unref?.();
    this.hangProbe = { id, sentAt: this.clock.now(), timer };
    this.logVia(
      `Hang watchdog: child silent past its window; sending liveness probe id=${id} probe_timeout_ms=${this.hangProbeTimeoutMs}; ${this.hangAttribution(hang)}`,
    );
    // A probe that cannot be written goes unanswered, and its timeout decides.
    try {
      this.process?.stdin?.write(`${JSON.stringify({ id, command: "ping" })}\n`, () => {});
    } catch {
      // Same as above: the probe timeout handles it.
    }
  }

  /**
   * The child answered the probe, so it can still respond. Requests whose
   * callers already gave up (timed out or aborted) stop counting as evidence:
   * their missing replies alone are no reason to replace a responsive child.
   * Requests whose callers are still waiting stay, and the silence window
   * restarts from this answer.
   */
  private onHangProbeAnswered(): void {
    const probeId = this.hangProbe?.id;
    this.clearHangProbe();
    let dropped = 0;
    for (const id of Array.from(this.awaitingReply.keys())) {
      if (this.pending.has(id)) continue;
      this.awaitingReply.delete(id);
      dropped += 1;
    }
    this.logVia(
      `Hang watchdog: liveness probe id=${probeId} answered; child kept; dropped ${dropped} unanswered request(s) whose callers already settled, ${this.awaitingReply.size} still waiting`,
    );
    this.clearHangWatchdogTimer();
    this.armHangWatchdog();
  }

  /**
   * No answer to the probe within its timeout. If the child wrote anything
   * else since the probe was sent it is not silent, and the watchdog simply
   * starts over from that output. Otherwise it is hung and is killed.
   */
  private onHangProbeTimeout(probeId: string): void {
    const probe = this.hangProbe;
    if (!probe || probe.id !== probeId) return;
    this.hangProbe = null;
    if (!this.isAlive()) return;
    const hang = this.hangDeadline();
    if (!hang) return;
    if (this.lastChildActivity >= probe.sentAt) {
      this.armHangWatchdog();
      return;
    }
    const attribution = `${this.hangAttribution(hang)} probe_id=${probeId} probe_timeout_ms=${this.hangProbeTimeoutMs}`;
    this.killHungChild(attribution, this.awaitingReply.get(hang.oldestId)?.sessionId);
  }

  private killHungChild(attribution: string, sessionId: string | undefined): void {
    this.timeoutRestartAwaitingRecovery = true;
    this.spawnedBinaryFingerprint = null;
    const aborted = Array.from(this.pending, ([requestId, entry]) => ({
      request_id: requestId,
      tool: entry.tool,
      retry: isReadOnlyRetryCommand(entry.tool),
    }));
    const fullAttribution = `${attribution} aborted_requests=${JSON.stringify(aborted)}`;
    this.configureWarningClients.clear();
    const pending = Array.from(this.pending.values());
    this.pending.clear();
    for (const entry of pending) {
      this.clock.clearTimeout(entry.timer);
      entry.onSettled?.();
      const readOnly = isReadOnlyRetryCommand(entry.tool);
      entry.reject(
        new BridgeRestartedError(
          entry.tool,
          !readOnly,
          readOnly
            ? `${this.errorPrefix} bridge restarted: the child wrote nothing for longer than its hang window and did not answer a liveness probe; "${entry.tool}" is read-only and safe to retry; ${fullAttribution}`
            : `${this.errorPrefix} bridge restarted: the child wrote nothing for longer than its hang window and did not answer a liveness probe; the outcome of "${entry.tool}" is unknown — a mutation may already have been applied, inspect before repeating it; ${fullAttribution}`,
        ),
      );
    }
    // Forget outstanding background task ids: their removal hooks died with
    // the child (foreground polls were just rejected and won't resume, and a
    // completion frame can't arrive from a killed process), so keeping them
    // would pin this bridge against idle eviction forever. Safe to forget —
    // detached tasks persist undelivered completions on disk, and the next
    // spawn rehydrates and delivers them.
    this.outstandingBackgroundTaskIds.clear();
    this.resetHangWatchdog();
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
    // "Bridge killed after timeout" is the phrase log tooling groups on.
    const killedMsg = tail
      ? `Bridge killed after timeout of the hang watchdog; ${fullAttribution}.${tail}`
      : `Bridge killed after timeout of the hang watchdog; ${fullAttribution} (see ${this.getLogFilePathVia()})`;
    if (sessionId) {
      this.sessionErrorVia(sessionId, killedMsg);
    } else {
      this.errorVia(killedMsg);
    }
  }

  private handleCrash(cause?: Error): void {
    this.clearRestartTimer();
    const proc = this.process;
    this.process = null;
    this.spawnedBinaryFingerprint = null;
    if (proc && proc.exitCode === null && !proc.killed) {
      proc.kill("SIGKILL");
    }
    this.clearRestartResetTimer();
    this.configured = false; // Force reconfigure on next command after restart
    // Forget outstanding background task ids — same rationale as
    // killHungChild: abandoned removal hooks would otherwise pin this bridge
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

      this.restartTimer = setTimeout(() => {
        this.restartTimer = null;
        if (!this._shuttingDown && !this.isAlive()) {
          try {
            this.spawnProcess();
          } catch (err) {
            this.errorVia(`Failed to restart: ${(err as Error).message}`);
          }
        }
      }, delay);
      // The crash already rejected every pending request, so nothing waits on
      // this restart; it must not keep a host that is otherwise done alive.
      this.restartTimer.unref?.();
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

  private rejectAllPending(error: BridgeTransportUnavailableError): void {
    this.configureWarningClients.clear();
    for (const [_id, entry] of this.pending) {
      this.clock.clearTimeout(entry.timer);
      entry.onSettled?.();
      entry.reject(error);
    }
    this.pending.clear();
  }

  private scheduleRestartCountReset(): void {
    if (this._restartCount === 0) return;
    this.clearRestartResetTimer();
    this.restartResetTimer = setTimeout(() => {
      this._restartCount = 0;
      this.restartResetTimer = null;
    }, BinaryBridge.RESTART_RESET_MS);
    // Bookkeeping only: a five-minute counter reset must not hold the host open.
    this.restartResetTimer.unref?.();
  }

  private clearRestartTimer(): void {
    if (this.restartTimer) clearTimeout(this.restartTimer);
    this.restartTimer = null;
  }

  private clearRestartResetTimer(): void {
    if (this.restartResetTimer) {
      clearTimeout(this.restartResetTimer);
      this.restartResetTimer = null;
    }
  }
}
