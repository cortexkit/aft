import {
  type AftProjectTransport,
  BASH_HOST_FALLBACK_REFUSAL,
  BASH_RUNON_DESCRIPTION,
  BASH_RUNON_GUIDANCE,
  type BridgeRequestOptions,
  bashHostFallbackAskPattern,
  classifyBashHostFallbackError,
  coerceBoolean,
  formatWatchWaited,
  interruptedWatchTail,
  isBridgeTransportTimeout,
  isTerminalStatus,
  LONGEST_TIMER_DELAY_MS,
  maxWatchTimeoutMs,
  maybeAppendConflictsHint,
  maybeAppendGrepSearchHint,
  resolveWatchTimeoutMs,
  runBashHostFallback,
  runningTaskStatusHint,
  taskKillDeadlineText,
  taskKillDeadlineWithinHandoffMargin,
  WATCH_SYNC_DEFAULTS_DESCRIPTION,
  WATCH_TIMEOUT_PARAM_DESCRIPTION,
  WATCH_UNAVAILABLE_GIVE_UP_MS,
  type WatchCallerRole,
  watchClock,
  watchPollDelayMs,
  watchTimeoutSteer,
  watchUnavailableSteer,
  workerBackgroundTaskNote,
  workerWatchStillRunning,
} from "@cortexkit/aft-bridge";
import type {
  AgentToolResult,
  ExtensionAPI,
  ExtensionContext,
  Theme,
} from "@earendil-works/pi-coding-agent";
import { Container, Spacer, Text } from "@earendil-works/pi-tui";
import { type Static, Type } from "typebox";
import {
  consumeBgCompletion,
  markBgCompletionDelivered,
  markExplicitControl,
  markTaskWaiting,
  trackBgTask,
  unmarkExplicitControl,
  unmarkTaskWaiting,
} from "../bg-notifications.js";
import { remoteRunsOffered, resolveBashConfig, toolEnabled } from "../config.js";
import { isPiWorkerSession } from "../session-kind.js";
import { clearSyncWatchAbort, isSyncWatchAborted } from "../sync-watch-abort.js";
import type { PluginContext } from "../types.js";
import {
  BridgeError,
  bridgeFor,
  callBridge,
  coerceOptionalInt,
  optionalInt,
  resolveSessionId,
  textResult,
} from "./_shared.js";
import { asString, collapsibleResult, type RenderResultOptionsLike } from "./render-helpers.js";

/** Only the UI preview is bounded; the final response still carries the complete output. */
export function appendBashStreamTail(previous: string, chunk: string): string {
  const cap = 64 * 1024;
  return (previous + chunk.slice(-cap)).slice(-cap);
}

const REGEX_WAIT_SCAN_WINDOW_BYTES = 64 * 1024;

/**
 * Decide whether a bash_watch caller should be treated as a delegated worker.
 * The classification itself lives in session-kind.ts so bash, bash_watch and
 * extension startup read the same signals.
 */
export function watchCallerRole(
  extCtx: Pick<ExtensionContext, "hasUI"> | undefined,
  env: NodeJS.ProcessEnv = process.env,
): WatchCallerRole {
  return isPiWorkerSession(extCtx, env) ? "worker" : "primary";
}

function coerceConfiguredWatchTimeout(
  value: unknown,
  role: WatchCallerRole,
  cap: number,
): number | undefined {
  try {
    return coerceOptionalInt(value, "timeoutMs", 1, maxWatchTimeoutMs(role, cap));
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    // Only a primary's timeout is bounded by the configured cap.
    if (role === "worker") throw error;
    throw new Error(`${message} (bash.watch_sync_max_ms)`);
  }
}

// Test-only override for the foreground wait window. Production resolves the
// window from config (floored at 5000ms), but bun caps each test at 5000ms, so
// promotion tests need a sub-floor window to exercise the promote path
// deterministically. Mirrors the Rust `AFT_CALLGRAPH_BUILD_WAIT_MS` test seam.
// Never set outside tests.
function resolveForegroundWaitMs(configured: number): number {
  const override = process.env.AFT_TEST_FOREGROUND_WAIT_MS;
  if (override !== undefined) {
    const parsed = Number(override);
    if (Number.isFinite(parsed) && parsed >= 0) return parsed;
  }
  return configured;
}
// Baseline bridge transport budget for bash-family control calls. The main
// orchestrated bash tool overrides this per request because Rust may hold the
// final response until the foreground wait window or hard-kill cap elapses.
const BASH_TRANSPORT_TIMEOUT_MS = 30_000;
const DEFAULT_HARD_TIMEOUT_MS = 30 * 60 * 1000;
// The margin gives Rust time to promote or finalize the task and deliver the
// final response after the server's foreground wait window or hard kill timeout.
const BASH_TRANSPORT_MARGIN_MS = 10_000;

function orchestratedTransportTimeoutMs(
  blockToCompletion: boolean,
  wait: boolean,
  effectiveTimeout: number | undefined,
  foregroundWaitMs: number,
  workerWaitMaxMs?: number,
): number {
  const blocking = blockToCompletion || wait;
  let waitBudget = blocking ? (effectiveTimeout ?? DEFAULT_HARD_TIMEOUT_MS) : foregroundWaitMs;
  // AFT returns every blocking call at bash.worker_wait_max_ms, even if
  // renewing the default kill deadline lets the command run longer.
  if (blocking && workerWaitMaxMs !== undefined) {
    waitBudget = Math.min(effectiveTimeout ?? workerWaitMaxMs, workerWaitMaxMs);
  }
  // A blocking `runon` call needs this same budget and no more: the engine
  // hands a remote task back at it (never later than 30 minutes) even while
  // the job is still queued on the runner, because a remote command's
  // `timeout` only starts once the runner runs it. The margin covers that reply.
  // A configured limit can exceed what a JavaScript timer accepts.
  return Math.min(waitBudget + BASH_TRANSPORT_MARGIN_MS, LONGEST_TIMER_DELAY_MS);
}

// Background task completion metadata shape (from Track D)
interface BgCompletion {
  task_id: string;
  status: "completed" | "failed" | "cancelled";
  exit_code?: number;
  command?: string;
}

// BashSpawnHook type — Pi's extension point for modifying bash execution
interface BashSpawnContext {
  command: string;
  cwd?: string;
  env?: Record<string, string>;
}

type BashSpawnHook = (ctx: BashSpawnContext) => BashSpawnContext | Promise<BashSpawnContext>;

const BashBaseParams = {
  command: Type.String({
    description: "Shell command to execute. Supports pipes, redirections, and shell syntax.",
  }),
  timeout: optionalInt(1, Number.MAX_SAFE_INTEGER, "Hard kill timeout in milliseconds"),
  workdir: Type.Optional(
    Type.String({
      description:
        "Working directory for command execution. Relative paths resolve against the project root. Defaults to the current session's working directory.",
    }),
  ),
  description: Type.Optional(
    Type.String({
      description:
        "Human-readable description shown in UI logs. Helps users understand what the command does without reading shell syntax.",
    }),
  ),
};

/**
 * `timeout` as shown while background tasks exist. It says up front that a
 * background task without a timeout is killed after 30 minutes, so a caller
 * can plan for it before launching. A fixed phrase, not the configured value,
 * so the prompt is the same for every user.
 */
const BashBackgroundTimeoutParam = {
  timeout: optionalInt(
    1,
    Number.MAX_SAFE_INTEGER,
    "Hard kill timeout in milliseconds. A background task with no timeout is killed after 30 minutes; pass a longer timeout for long jobs.",
  ),
};

const BashWaitParam = {
  wait: Type.Optional(
    Type.Boolean({
      description:
        "When true, run in the foreground without auto-promoting and wait until the command finishes or reaches its timeout (at most bash.worker_wait_max_ms, 30 minutes by default; the command then keeps running in the background and the reply says how to keep waiting); any new message detaches by default, while `bash.detach_on_user_message: false` keeps it blocking unless the message contains the literal `&detach`. The token is stripped before delivery; the rest of the message is preserved, and a token-only message becomes `(requested background detach)`. Use only when you know the result is required before doing anything else.",
    }),
  ),
};

const BashSandboxParam = {
  sandbox: Type.Optional(
    Type.Literal("host", {
      description:
        "Request one-command approval to run unsandboxed on the host; use only when native sandboxing blocks required work, and note that it is a no-op when sandboxing is disabled.",
    }),
  ),
};

const BashBackgroundFlagParam = {
  background: Type.Optional(
    Type.Boolean({
      description:
        "Spawn command in background and return immediately with a task_id. Use bash_watch to wait for completion or output patterns; bash_status for a one-shot snapshot only. Use bash_kill to terminate. Ideal for long-running tasks like builds or dev servers.",
    }),
  ),
};

const BashCompressionParam = {
  compressed: Type.Optional(
    Type.Boolean({
      description:
        "Compress output by removing ANSI codes, carriage returns, and excessive blank lines. Default: true. Set to false for raw terminal output including color codes.",
    }),
  ),
};

const BashPtyParams = {
  pty: Type.Optional(
    Type.Boolean({
      description:
        'Spawn the command in a real PTY for interactive programs. Implies background: true automatically. Inspect with bash_status({ task_id, output_mode: "screen" }) and send input with bash_write.',
    }),
  ),
  ptyRows: optionalInt(1, 60, "PTY terminal height in rows (minimum 1, maximum 60)"),
  ptyCols: optionalInt(1, 140, "PTY terminal width in columns (minimum 1, maximum 140)"),
};

const BashRunonParam = {
  runon: Type.Optional(Type.String({ description: BASH_RUNON_DESCRIPTION })),
};

/** The full argument set, used for the parameter types `execute` receives. */
const BashParams = Type.Object({
  ...BashBaseParams,
  ...BashWaitParam,
  ...BashSandboxParam,
  ...BashBackgroundFlagParam,
  ...BashCompressionParam,
  ...BashPtyParams,
  ...BashRunonParam,
});

/**
 * The argument set a model is shown: each optional argument exists only while
 * the feature it controls is on, so no knob that does nothing is offered.
 * `wait`, `background` and the PTY arguments need `bash.background`,
 * `compressed` needs `bash.compress`, `sandbox` needs `sandbox.enabled`, and
 * `runon` needs subc mode with remote runs enabled in the user config (bash
 * only, never PowerShell). A stale call that still sends a removed argument
 * is ignored in `execute`, except `runon`, which is forwarded so the engine
 * refuses it by name rather than running the command locally.
 * Keys keep the order of the full set, so the all-on schema is unchanged.
 */
function bashParamsForConfig(
  features: {
    background: boolean;
    compress: boolean;
    sandbox: boolean;
    subagentBackground: boolean;
    runon?: boolean;
  },
  companions: RegisteredCompanions,
): typeof BashParams {
  return Type.Object({
    ...BashBaseParams,
    // Replaces the base `timeout` in place (the key keeps its position).
    ...(features.background ? BashBackgroundTimeoutParam : {}),
    ...(features.background ? BashWaitParam : {}),
    ...(features.sandbox ? BashSandboxParam : {}),
    ...(features.background
      ? {
          background: Type.Optional(
            Type.Boolean({ description: backgroundParamDescription(companions) }),
          ),
        }
      : {}),
    ...(features.compress ? BashCompressionParam : {}),
    ...(features.background
      ? {
          ...BashPtyParams,
          pty: Type.Optional(
            Type.Boolean({
              description: ptyParamDescription(companions, features.subagentBackground),
            }),
          ),
        }
      : {}),
    ...(features.runon ? BashRunonParam : {}),
  }) as unknown as typeof BashParams;
}

/** Which bash companions the model can call; descriptions name only these. */
interface RegisteredCompanions {
  status: boolean;
  watch: boolean;
  write: boolean;
  kill: boolean;
}

/**
 * Whether a bash companion tool is registered for this configuration: it
 * needs `bash.background` and must not be in `disabled_tools`.
 */
export function bashCompanionRegistered(
  config: PluginContext["config"],
  companion: "bash_status" | "bash_watch" | "bash_write" | "bash_kill",
): boolean {
  return resolveBashConfig(config).background && toolEnabled(config, companion);
}

function registeredCompanions(config: PluginContext["config"]): RegisteredCompanions {
  return {
    status: bashCompanionRegistered(config, "bash_status"),
    watch: bashCompanionRegistered(config, "bash_watch"),
    write: bashCompanionRegistered(config, "bash_write"),
    kill: bashCompanionRegistered(config, "bash_kill"),
  };
}

function backgroundParamDescription(c: RegisteredCompanions): string {
  let controls = "";
  if (c.watch) {
    controls += ` Use bash_watch to wait for completion or output patterns${c.status ? "; bash_status for a one-shot snapshot only." : "."}`;
  } else if (c.status) {
    controls += " Use bash_status for a one-shot snapshot.";
  }
  if (c.kill) controls += " Use bash_kill to terminate.";
  return `Spawn command in background and return immediately with a task_id.${controls} Ideal for long-running tasks like builds or dev servers. A background task with no timeout is killed after 30 minutes; pass a longer timeout for long jobs.`;
}

/**
 * A PTY session only exists as a background task, so with
 * `bash.subagent_background: false` worker sessions refuse `pty: true`; the
 * description says so only in that configuration.
 */
function ptyParamDescription(c: RegisteredCompanions, subagentBackground: boolean): string {
  let drive = "";
  if (c.status && c.write) {
    drive =
      ' Inspect with bash_status({ task_id, output_mode: "screen" }) and send input with bash_write.';
  } else if (c.status) {
    drive = ' Inspect with bash_status({ task_id, output_mode: "screen" }).';
  } else if (c.write) {
    drive = " Send input with bash_write.";
  }
  const workers = subagentBackground
    ? ""
    : " Unavailable in worker sessions because bash.subagent_background is false.";
  return `Spawn the command in a real PTY for interactive programs. Implies background: true automatically.${workers}${drive}`;
}

/**
 * Whether the native bash sandbox is on for this configuration. The `sandbox`
 * argument only asks to leave that sandbox for one command, so it is offered
 * only when there is a sandbox to leave.
 */
export function nativeSandboxEnabled(config: PluginContext["config"]): boolean {
  return config.sandbox?.enabled === true;
}

const BashTaskParams = Type.Object({
  task_id: Type.String({
    description: "Background bash task id returned by bash({ background: true }).",
  }),
});

const BashStatusParams = Type.Object({
  task_id: Type.String({
    description: "Background bash task id returned by bash({ background: true }).",
  }),
  output_mode: Type.Optional(
    Type.Union([Type.Literal("screen"), Type.Literal("raw"), Type.Literal("both")], {
      description:
        "PTY output rendering mode. Defaults to screen for PTY tasks and preserves existing behavior for piped tasks when omitted.",
    }),
  ),
});

const BashWatchParams = Type.Object({
  task_id: Type.String({
    description: "Background bash task id returned by bash({ background: true }).",
  }),
  pattern: Type.Optional(Type.Union([Type.String(), Type.Object({ regex: Type.String() })])),
  background: Type.Optional(Type.Boolean()),
  timeout_ms: optionalInt(1, 1800000, WATCH_TIMEOUT_PARAM_DESCRIPTION),
  once: Type.Optional(Type.Boolean()),
});

const BashWriteParams = Type.Object({
  task_id: Type.String({
    description: "Background PTY task id returned by bash({ pty: true, background: true }).",
  }),
  // input accepts either a plain string (verbatim bytes) or a sequence array
  // mixing strings (text) with { key: "<name>" } objects (named control keys).
  // Rust validates each item; unknown key names return invalid_request.
  input: Type.Union(
    [
      Type.String(),
      Type.Array(
        Type.Union([
          Type.String(),
          Type.Object({
            key: Type.String({
              description:
                "Named control key, e.g. 'esc', 'enter', 'up', 'ctrl-c'. Case-insensitive.",
            }),
          }),
        ]),
      ),
    ],
    {
      description:
        "Either a string of verbatim bytes (e.g. 'print(1)\\n') OR an array mixing strings " +
        "and { key: '<name>' } objects for atomic text+key sequences. " +
        "Example: [ 'iHello', { key: 'esc' }, ':wq', { key: 'enter' } ]. " +
        "Allowed key names: enter, return, tab, space, backspace, esc, escape, up, down, " +
        "left, right, home, end, page-up, page-down, delete, insert, f1..f12, ctrl-a..ctrl-z.",
    },
  ),
});

interface BashDetails {
  exit_code?: number;
  duration_ms?: number;
  truncated?: boolean;
  output_path?: string;
  task_id?: string;
  bg_completions?: BgCompletion[];
}

interface BashStatusWaited {
  reason: "matched" | "exited" | "timeout" | "user_message" | "unavailable" | "aborted";
  /** Real time the watch held the call, measured on a monotonic clock. */
  elapsed_ms: number;
  /** Longest time this watch was allowed to hold the call; absent when it had no deadline. */
  limit_ms?: number;
  match?: string;
  match_offset?: number;
  match_stream?: "stdout" | "stderr";
}

/**
 * Minimal snapshot used when a watch ends without ever reading task status
 * (the bridge stayed busy past our deadline). Keeps the formatter from
 * dereferencing an absent snapshot.
 */
function unavailableSnapshot(): Record<string, unknown> {
  return { status: "unknown" };
}

interface BashStatusDetails {
  success: boolean;
  status: string;
  exit_code?: number;
  duration_ms?: number;
  output_preview?: string;
  output_incomplete?: boolean;
  status_reason?: string;
  command?: string;
  mode?: string;
  output_path?: string;
  pty_rows?: number;
  pty_cols?: number;
  pty_screen?: string;
  pty_raw?: string;
  live_descendants?: Array<{ pid: number; comm: string; argv0: string }> | null;
  live_descendants_omitted?: number;
  live_descendants_summary?: string;
  waited?: BashStatusWaited;
}

interface BashWriteDetails {
  success: boolean;
  bytes_written?: number;
}

interface BashKillDetails {
  success: boolean;
  status: string;
  kill_signaled?: boolean;
  kill_reached?: number;
}

interface BashWatchDetails extends Record<string, unknown> {}

/** Local shape for Pi's render context — mirrors hoisted.ts pattern. */
interface RenderContextLike {
  lastComponent: import("@earendil-works/pi-tui").Component | undefined;
  isError: boolean;
}

async function callBashBridge(
  bridge: AftProjectTransport,
  command: string,
  params: Record<string, unknown> = {},
  extCtx?: ExtensionContext,
  options?: BridgeRequestOptions,
): Promise<Record<string, unknown>> {
  return await callBridge(bridge, command, params, extCtx, {
    transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS,
    ...options,
    keepBridgeOnTimeout: true,
  });
}

interface BashPermissionAsk {
  kind?: string;
  command?: string;
  cwd?: string;
  grant_id?: string;
  patterns?: string[];
  always?: string[];
}

function piEscalationAskText(ask: BashPermissionAsk): string {
  return `This command will run UNSANDBOXED on the host.\n\nExact command:\n${ask.command ?? ""}\n\nWorking directory:\n${ask.cwd ?? ""}`;
}

function piPowerShellAskText(command: string): string {
  return `PowerShell commands are conservatively approved one at a time because AFT's POSIX shell scanner cannot safely interpret PowerShell syntax.\n\nExact command:\n${command}`;
}

async function callBashWithPermissionLoop(
  bridge: AftProjectTransport,
  params: Record<string, unknown>,
  extCtx: ExtensionContext,
  options?: BridgeRequestOptions,
  bridgeCommand = "bash",
): Promise<Record<string, unknown>> {
  const granted = Array.isArray(params.permissions_granted)
    ? params.permissions_granted.filter((value): value is string => typeof value === "string")
    : [];

  for (let round = 0; round < 8; round++) {
    try {
      return await callBashBridge(
        bridge,
        bridgeCommand,
        { ...params, ...(granted.length > 0 ? { permissions_granted: granted } : {}) },
        extCtx,
        options,
      );
    } catch (error) {
      if (!(error instanceof BridgeError)) {
        if (error instanceof Error && error.message.includes("permission_required")) {
          throw new Error("Permission ask reached Pi adapter — this is a bug.");
        }
        throw error;
      }
      if (error.code !== "permission_required") throw error;
      const asks = Array.isArray(error.response?.asks)
        ? (error.response.asks as BashPermissionAsk[])
        : [];
      if (asks.length === 0 || !extCtx.hasUI || typeof extCtx.ui?.confirm !== "function") {
        throw new BridgeError(
          "Permission denied: command approval requires an interactive UI.",
          "permission_denied",
        );
      }
      for (const ask of asks) {
        if (ask.kind === "escalation") {
          const approved = await extCtx.ui.confirm(
            "Run command unsandboxed on host?",
            piEscalationAskText(ask),
            { signal: extCtx.signal },
          );
          if (!approved) {
            throw new BridgeError(
              "Permission denied: unsandboxed host execution was denied.",
              "permission_denied",
            );
          }
          if (ask.grant_id && !granted.includes(ask.grant_id)) granted.push(ask.grant_id);
          continue;
        }
        if (bridgeCommand !== "powershell") {
          throw new Error(
            "Permission ask reached Pi adapter without a host escalation grant — this is a bug.",
          );
        }
        const approved = await extCtx.ui.confirm(
          "Allow PowerShell command?",
          piPowerShellAskText(String(params.command ?? "")),
          { signal: extCtx.signal },
        );
        if (!approved) {
          throw new BridgeError(
            "Permission denied: PowerShell command was denied.",
            "permission_denied",
          );
        }
        for (const grant of [...(ask.always ?? []), ...(ask.patterns ?? [])]) {
          if (!granted.includes(grant)) granted.push(grant);
        }
      }
    }
  }

  throw new Error("bash permission retry failed: too many rounds");
}

/** Truncate output to last N visual lines for terminal width. */
function truncateToVisualLines(text: string, maxLines: number): string {
  const lines = text.split("\n");
  if (lines.length <= maxLines) return text;
  return lines.slice(-maxLines).join("\n");
}

/** Reuse a compatible Text component from last render, or create fresh. */
function reuseText(last: import("@earendil-works/pi-tui").Component | undefined): Text {
  return last instanceof Text ? last : new Text("", 0, 0);
}

/** Reuse a compatible Container from last render, or create fresh. */
function reuseContainer(last: import("@earendil-works/pi-tui").Component | undefined): Container {
  return last instanceof Container ? last : new Container();
}

/** Extract BashSpawnHook from ExtensionAPI if available. */
function getBashSpawnHook(pi: ExtensionAPI): BashSpawnHook | undefined {
  // Pi exposes hooks via getHook() or similar — defensive access
  const api = pi as unknown as {
    getHook?: (name: string) => BashSpawnHook | undefined;
    hooks?: { bashSpawn?: BashSpawnHook };
  };
  if (typeof api.getHook === "function") {
    return api.getHook("bashSpawn");
  }
  return api.hooks?.bashSpawn;
}

/**
 * How the description tells the agent to wait on a background task, naming
 * `bash_watch` and `bash_status` only when they are registered.
 */
function backgroundWaitSentence(c: RegisteredCompanions): string {
  if (c.watch) {
    const noPolling = c.status ? ", and never loop `bash_status` to wait" : "";
    return `then \`bash_watch\` handles only a short remaining wait (in a main session a watch defaults to 30s, max bash.watch_sync_max_ms, 120s by default; in a delegated session a watch without a timeout waits up to the worker wait limit, bash.worker_wait_max_ms, 30 minutes by default, then reports it is still running; watch again to keep waiting); for anything longer end the turn and let the completion reminder wake you, or use bash({wait:true}) when the result is needed before anything else — never background a command and immediately \`bash_watch\` it (that wastes a turn for what foreground returns in one)${noPolling}.`;
  }
  return c.status
    ? "the task keeps running after the call returns, a completion reminder arrives when it exits, and `bash_status` reports its state and output. Use bash({wait:true}) instead when the result is needed before anything else."
    : "the task keeps running after the call returns and a completion reminder arrives when it exits. Use bash({wait:true}) instead when the result is needed before anything else.";
}

/** How a PTY session is driven, naming only the companions that exist. */
function ptyDriveClause(c: RegisteredCompanions): string {
  if (c.status && c.write) {
    return ', and is driven with `bash_status({ output_mode: "screen" })` plus `bash_write`';
  }
  if (c.status) return ', and its screen is read with `bash_status({ output_mode: "screen" })`';
  if (c.write) return ", and is driven with `bash_write`";
  return "";
}

/** Register AFT's primary bash tool under either the host or aft_ name. */
export function registerBashTool(
  pi: ExtensionAPI,
  ctx: PluginContext,
  aftSearchRegistered = false,
  registeredName = "bash",
  registerCompanions = true,
  shell: "bash" | "powershell" = "bash",
): void {
  const isPowerShell = shell === "powershell";
  const spawnHook = isPowerShell ? undefined : getBashSpawnHook(pi);
  const readToolName = "read";
  const grepToolName = "grep";
  // Agent-facing wording: no internal vocabulary ("hoisted", "Rust handler",
  // "command rewriting") — describe what the tool does and what NOT to use it
  // for. The code-search prohibition steers to aft_search when registered,
  // else to the grep tool (same surface logic as the Rust grep footer). The
  // compression sentence only appears when compression is actually on —
  // advertising `compressed: false` when compression is disabled would
  // describe a no-op. Background/PTY/watch wording appears only when
  // `bash.background` is enabled.
  const zoomSteer = toolEnabled(ctx.config, "aft_zoom") ? ", or `aft_zoom`" : "";
  const steerTools = [
    aftSearchRegistered
      ? "`aft_search` (concepts, identifiers, regex, literals)"
      : `the \`${grepToolName}\` tool`,
    `\`${readToolName}\``,
    ...(toolEnabled(ctx.config, "aft_outline") ? ["`aft_outline`"] : []),
  ].join(", ");
  const searchSteer = `use ${steerTools}${zoomSteer} instead`;
  // Companion names appear in this tool's text only when the model can call them.
  const companions = registeredCompanions(ctx.config);
  const bashCfg = resolveBashConfig(ctx.config);
  // Every command probes the module before fallback. Retain only enough state to
  // clear fallback mode after the first successful module response.
  let hostFallbackActive = false;
  const compressionSentence = bashCfg.compress
    ? " Output is compressed by default; pass `compressed: false` for raw output. Piped commands run verbatim and show the pipeline's output; for AFT's test/build summary, run the runner without `| head`, `| tail`, or `| grep`."
    : "";
  // The prompt snippet lists what the call supports, so each entry follows the
  // feature that provides it.
  const supported = bashCfg.background
    ? [
        "workdir",
        "background tasks",
        ...(bashCfg.compress ? ["compressed output"] : []),
        "PTY mode",
      ].join(", ")
    : bashCfg.compress
      ? "workdir and compressed output"
      : "workdir";
  const detachSentence = bashCfg.detach_on_user_message
    ? "Any new message detaches this wait. Set `bash.detach_on_user_message: false` to keep it blocking; even then, a message containing the literal `&detach` forces detachment, and the token is stripped before delivery; the rest of the message is preserved, while a token-only message becomes `(requested background detach)`."
    : "Because `bash.detach_on_user_message` is false, a new message leaves this wait blocking; include the literal `&detach` anywhere to force detachment, and the token is stripped before delivery; the rest of the message is preserved, while a token-only message becomes `(requested background detach)`.";
  const tasksSentence = bashCfg.background
    ? ` Commands run in the foreground and return inline; \`wait: true\` blocks until a long command finishes instead of auto-promoting (it blocks up to \`bash.worker_wait_max_ms\`, 30 minutes by default, then reports the command is still running; watch again to keep waiting); ${detachSentence} Use it when you need the result before doing anything else; keep it off otherwise so auto-promote can remind you while you work. Use \`background: true\` yourself ONLY when you have other useful work to do while it runs; ${backgroundWaitSentence(companions)} A \`nohup … &\` launch still holds the call if the child keeps stdout/stderr; redirect both or use background:true. \`pty: true\` runs interactive programs (REPLs, TUIs), implies background${ptyDriveClause(companions)}.`
    : " Commands run in the foreground to completion; `timeout` is the hard kill cap (default 30 minutes).";
  const remoteRuns = () => !isPowerShell && remoteRunsOffered(ctx.config);
  pi.registerTool<typeof BashParams, BashDetails>({
    name: registeredName,
    label: registeredName,
    get description() {
      return isPowerShell
        ? `Execute PowerShell commands through AFT.${compressionSentence}${tasksSentence}\n\nPowerShell syntax is not analyzed as POSIX shell. Each command requires explicit approval so syntax AFT cannot safely interpret is never auto-allowed.`
        : `Execute shell commands.${compressionSentence}${tasksSentence} \`timeout\` starts after spawn and kills the Unix process group (exit 124; Windows uses taskkill /T /F); processes that leave the group survive.${bashCfg.background ? " Finished output expires after the task is 24 hours old and its completion has been delivered; under project-root restrictions, only the starting session can read output outside the project—copy cited lines into your report." : ""}${remoteRuns() ? ` ${BASH_RUNON_GUIDANCE}` : ""}\n\nDO NOT use bash for code search or code exploration. If you are about to run grep, rg, sed, awk, find, or cat through bash to locate or read code: STOP — ${searchSteer}. When a list is cut, the reply ends with \`shown N of M <unit> (<reason>) · narrow: <knobs>\`; absence of that line means the list is complete.`;
    },
    promptSnippet: isPowerShell
      ? `Run PowerShell commands (timeout in milliseconds; supports ${supported})`
      : `Run shell commands (timeout in milliseconds; supports ${supported})`,
    promptGuidelines: isPowerShell
      ? ["Use PowerShell syntax. Every command requires explicit approval."]
      : [
          `DO NOT use bash for code search or exploration — ${searchSteer}.`,
          ...(bashCfg.compress
            ? ["Set compressed: false when you need ANSI color codes in the output."]
            : []),
          "Piped commands run verbatim and show the pipeline's output; run test/build tools without pipes when you need AFT's summary.",
        ],
    get parameters() {
      return bashParamsForConfig(
        {
          background: bashCfg.background,
          compress: bashCfg.compress,
          sandbox: nativeSandboxEnabled(ctx.config),
          subagentBackground: bashCfg.subagent_background,
          runon: remoteRuns(),
        },
        companions,
      );
    },
    async execute(_toolCallId, params: Static<typeof BashParams>, signal, onUpdate, extCtx) {
      if (params.runon !== undefined && !remoteRuns()) {
        throw new Error(
          "runon refused: remote execution or bash.runon_enabled is unavailable in this session",
        );
      }
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const bashCfg = resolveBashConfig(ctx.config);
      const foregroundWaitMs = resolveForegroundWaitMs(bashCfg.foreground_wait_window_ms);
      const backgroundDisabled = !bashCfg.background;
      // ptyRows/ptyCols are silently ignored when pty is false so agents
      // that defensively pass them on normal bash calls don't get stuck in
      // a retry loop. pty: true silently implies background: true (Rust
      // bash.rs handles the auto-promote); we mirror that here so the
      // Pi-side spawn payload also reflects the auto-promotion. When background
      // is disabled these params are omitted from the schema and defensively
      // ignored if a stale caller sends them anyway.
      //
      // `timeout` is the command's hard kill cap in every mode, forwarded
      // unchanged. On the default foreground path a cap shorter than the
      // foreground wait window kills the command before it would be promoted,
      // and the call answers with the timed-out result. Omitting it lets the
      // engine apply its 30-minute default.
      const timeout = coerceOptionalInt(params.timeout, "timeout", 1, Number.MAX_SAFE_INTEGER);
      const ptyRows = backgroundDisabled
        ? undefined
        : coerceOptionalInt(params.ptyRows, "ptyRows", 1, 60);
      const ptyCols = backgroundDisabled
        ? undefined
        : coerceOptionalInt(params.ptyCols, "ptyCols", 1, 140);
      const compressed = coerceBoolean(params.compressed, true);
      // With background off, `wait` is not in the schema and every command
      // already runs to completion. A stale `wait: true` is ignored rather than
      // forwarded: the engine would otherwise make the call detachable on the
      // next user message, which would move it into a background task. The
      // contradiction checks below therefore only fire while background is on.
      const requestedWait = !backgroundDisabled && coerceBoolean(params.wait);
      const rawRequestedPty = coerceBoolean(params.pty);
      const rawRequestedBackground = coerceBoolean(params.background);
      if (requestedWait && rawRequestedPty) {
        throw new Error(
          "wait:true cannot be used with pty:true because PTY sessions run in background.",
        );
      }
      if (requestedWait && rawRequestedBackground) {
        throw new Error("wait:true cannot be used with background:true.");
      }
      // Coerce at the boundary: stringified pty/background flags (coerceBoolean).
      const requestedPty = !backgroundDisabled && rawRequestedPty;
      // With `bash.subagent_background: false` a worker session runs every
      // command to completion, as on OpenCode: a headless or delegated Pi run
      // ends with its turn, so a backgrounded task could never report back.
      const workerForcedForeground = !bashCfg.subagent_background && isPiWorkerSession(extCtx);
      if (workerForcedForeground && requestedPty) {
        // A PTY session only exists as a background task, which this setting
        // rules out; running it in the foreground would just sit on the
        // interactive program until the hard timeout.
        throw new Error(
          "pty:true is unavailable in this worker session because bash.subagent_background is false; run the command without pty.",
        );
      }
      const blockToCompletion = backgroundDisabled || requestedWait || workerForcedForeground;
      const effectiveBackground = !blockToCompletion && (rawRequestedBackground || requestedPty);
      const isWorker = isPiWorkerSession(extCtx);
      // AFT hands every blocking call back at bash.worker_wait_max_ms with
      // the command still running. The bridge's reply timeout must allow
      // that wait so it does not lose the handoff reply.
      const workerWaitMaxMs = bashCfg.worker_wait_max_ms;

      // Build spawn context for potential hook modification
      let spawnContext: BashSpawnContext = {
        command: params.command,
        cwd: params.workdir,
      };

      // Apply BashSpawnHook if available (Pi extension point)
      if (spawnHook) {
        try {
          spawnContext = await spawnHook(spawnContext);
        } catch (hookErr) {
          // Hook errors should not silently fail — surface them
          throw new Error(
            `BashSpawnHook failed: ${hookErr instanceof Error ? hookErr.message : String(hookErr)}`,
          );
        }
      }

      const bridgeCommand = spawnContext.command;

      let streamed = "";
      let usedHostFallback = false;
      let response: Record<string, unknown>;
      try {
        response = await callBashWithPermissionLoop(
          bridge,
          {
            command: bridgeCommand,
            timeout,
            workdir: spawnContext.cwd ?? params.workdir,
            env: spawnContext.env,
            description: params.description,
            background: effectiveBackground,
            notify_on_completion: effectiveBackground,
            compressed,
            pty: requestedPty,
            pty_rows: ptyRows,
            pty_cols: ptyCols,
            foreground_orchestrate: true,
            block_to_completion: blockToCompletion,
            wait: requestedWait,
            sandbox: params.sandbox,
            ...(isPowerShell ? { shell: "powershell" } : {}),
            ...(params.runon !== undefined ? { runon: params.runon } : {}),
          },
          extCtx,
          {
            transportTimeoutMs: orchestratedTransportTimeoutMs(
              blockToCompletion,
              requestedWait,
              timeout,
              foregroundWaitMs,
              workerWaitMaxMs,
            ),
            onProgress: ({ text }) => {
              streamed = appendBashStreamTail(streamed, text);
              // Stream truncated output to avoid overwhelming the UI
              const displayText = truncateToVisualLines(streamed, 100);
              onUpdate?.(bashResult(displayText, { streaming: true }));
            },
          },
          isPowerShell ? "powershell" : "bash",
        );
      } catch (error) {
        const fallbackCause = classifyBashHostFallbackError(error);
        if (isPowerShell || !bashCfg.host_fallback || fallbackCause === undefined) throw error;
        if (!backgroundDisabled && rawRequestedBackground) {
          throw new Error(`${BASH_HOST_FALLBACK_REFUSAL}; background:true is unsupported.`);
        }
        if (requestedPty) {
          throw new Error(`${BASH_HOST_FALLBACK_REFUSAL}; pty:true is unsupported.`);
        }
        if (params.runon !== undefined) {
          throw new Error(
            `${BASH_HOST_FALLBACK_REFUSAL}; runon is unsupported, and the command was not run locally.`,
          );
        }
        if (!extCtx.hasUI || typeof extCtx.ui?.confirm !== "function") {
          throw new BridgeError(
            "Permission denied: host fallback execution requires an interactive UI.",
            "permission_denied",
          );
        }

        const projectRoot = extCtx.cwd;
        const pattern = bashHostFallbackAskPattern(bridgeCommand, projectRoot, fallbackCause);
        const approved = await extCtx.ui.confirm(
          "AFT unavailable — run command on host?",
          pattern,
          {
            signal: signal ?? extCtx.signal,
          },
        );
        if (!approved) {
          throw new BridgeError(
            "Permission denied: AFT host fallback execution was denied.",
            "permission_denied",
          );
        }
        response = await runBashHostFallback({
          command: bridgeCommand,
          projectRoot,
          timeoutMs: timeout,
          signal: signal ?? extCtx.signal,
          env: spawnContext.env,
        });
        usedHostFallback = true;
        hostFallbackActive = true;
      }

      if (response.success === false) {
        throw new Error((response.message as string | undefined) ?? "bash failed");
      }

      // The normal dispatch above is the foreground recovery probe. Once it
      // succeeds, later results must render without the host-fallback banner.
      if (!usedHostFallback && hostFallbackActive) hostFallbackActive = false;

      const taskId = response.task_id as string | undefined;
      if (response.status === "running" && taskId) {
        trackBgTask(resolveSessionId(extCtx), taskId);
        // The Rust engine words this hand-off text for the caller's role
        // (worker_session, which callBridge adds to every request). It cannot
        // assume the host has bash_watch, so a worker is told here how to
        // wait on the task it now holds.
        const handOff = (response.output as string | undefined) ?? "";
        return bashResult(
          isWorker ? handOff + workerBackgroundTaskNote(taskId, "task_id") : handOff,
          { task_id: taskId },
        );
      }

      const details: BashDetails = {
        exit_code: response.exit_code as number | undefined,
        duration_ms: response.duration_ms as number | undefined,
        truncated: response.truncated as boolean | undefined,
        output_path: response.output_path as string | undefined,
        task_id: taskId,
      };

      const output = (response.output as string | undefined) ?? "";
      return bashResult(
        usedHostFallback || isPowerShell
          ? output
          : withBashHints(output, bridgeCommand, aftSearchRegistered, extCtx.cwd),
        details,
      );
    },
    renderCall(args, theme, context) {
      return renderBashCall(asString(args?.command), asString(args?.description), theme, context);
    },
    renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
      return renderBashResult(result, theme, context, options);
    },
  });

  // Standalone registration remains convenient for direct consumers. The
  // production surface passes false and registers each companion by name.
  if (registerCompanions) registerBashCompanionTools(pi, ctx);
}

/**
 * Register controls for AFT-owned background task IDs. Each companion is an
 * independent registration controlled by its own name in `disabled_tools`;
 * the bash runtime gate is enforced by the engine. All four only act on
 * background tasks, so none is registered while `bash.background` is off.
 */
export function registerBashCompanionTools(
  pi: ExtensionAPI,
  ctx: PluginContext,
  enabled: {
    bashStatus: boolean;
    bashWatch: boolean;
    bashWrite: boolean;
    bashKill: boolean;
  } = { bashStatus: true, bashWatch: true, bashWrite: true, bashKill: true },
): void {
  if (!resolveBashConfig(ctx.config).background) return;
  if (enabled.bashStatus) {
    pi.registerTool<typeof BashStatusParams, BashStatusDetails>(createBashStatusTool(ctx));
  }
  if (enabled.bashWatch) {
    pi.registerTool<typeof BashWatchParams, BashWatchDetails>(createBashWatchTool(ctx));
  }
  if (enabled.bashWrite) {
    pi.registerTool<typeof BashWriteParams, BashWriteDetails>(createBashWriteTool(ctx));
  }
  if (enabled.bashKill) {
    pi.registerTool<typeof BashTaskParams, BashKillDetails>(createBashKillTool(ctx));
  }
}

/**
 * Append AFT bash-output hints (conflicts / grep) to a foreground bash result.
 * Pi knows the exact command, so the grep hint is matched against it directly
 * rather than the echoed first output line. Mirrors OpenCode's
 * `tool.execute.after` nudges; only fires on terminal bash output (not
 * background-spawn/promotion messages, which have no real output yet).
 */
function withBashHints(
  output: string,
  command: string,
  aftSearchRegistered: boolean,
  projectRoot: string,
): string {
  return maybeAppendGrepSearchHint(
    maybeAppendConflictsHint(output),
    command,
    aftSearchRegistered,
    projectRoot,
  );
}

export function createBashStatusTool(ctx: PluginContext) {
  return {
    name: "bash_status",
    label: "bash_status",
    // Point at bash_watch for waiting only when the model can call it.
    description: `Read-only snapshot of a background bash task. Returns immediately. Never waits. One look to check on a task is fine — never loop it to wait for completion.${bashCompanionRegistered(ctx.config, "bash_watch") ? " To wait, use bash_watch." : ""}`,
    promptSnippet: "Inspect a background bash task by task_id",
    parameters: BashStatusParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof BashStatusParams>,
      _signal: AbortSignal | undefined,
      _onUpdate: ((update: AgentToolResult<BashStatusDetails>) => void) | undefined,
      extCtx: ExtensionContext,
    ) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      // bash_status is snapshot-only. wait_for / exit / timeout_ms moved to
      // bash_watch; if the agent passes them here they're silently ignored
      // at the TypeBox schema layer.
      const data = await bashStatusSnapshot(bridge, extCtx, params.task_id, params.output_mode);
      const details = data as unknown as BashStatusDetails;
      return bashStatusResult(
        await formatBashStatus(extCtx, params.task_id, details, params.output_mode, {
          role: watchCallerRole(extCtx),
          capMs: resolveBashConfig(ctx.config).watch_sync_max_ms,
        }),
        details,
      );
    },
  };
}

export function createBashWatchTool(ctx: PluginContext) {
  return {
    name: "bash_watch",
    label: "bash_watch",
    // The polling warning names bash_status only when the model can call it.
    description: `Watch a background bash task. ${WATCH_SYNC_DEFAULTS_DESCRIPTION}. In a main session sync waits are for a short remaining wait on a task; for anything longer end the turn on \`bash({background:true})\` and let the completion reminder wake you, or use \`bash({wait:true})\` when the result is needed before anything else. The user can interrupt anytime; the wait auto-converts to an async notification. Async (background:true, requires pattern) registers a non-blocking notification and returns immediately — use when you have parallel work or want to end your turn.${bashCompanionRegistered(ctx.config, "bash_status") ? " Never loop bash_status to wait." : ""}`,
    promptSnippet: "Wait for or watch a background bash task",
    parameters: BashWatchParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof BashWatchParams>,
      signal: AbortSignal | undefined,
      _onUpdate: ((update: AgentToolResult<BashWatchDetails>) => void) | undefined,
      extCtx: ExtensionContext,
    ) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const waitFor = parseWaitPattern(params.pattern);
      const bashCfg = resolveBashConfig(ctx.config);
      // A worker that may not background also may not park an async watch it
      // will never be woken by; the watch becomes a sync wait like any worker
      // watch without a timeout, up to the worker wait limit.
      const workerForcedSync =
        coerceBoolean(params.background) &&
        !bashCfg.subagent_background &&
        isPiWorkerSession(extCtx);
      // Coerce at the boundary: stringified background must enable async mode (coerceBoolean).
      if (coerceBoolean(params.background) && !workerForcedSync) {
        if (!waitFor) {
          throw new Error(
            "invalid_request: Use auto-reminder; bash_watch without pattern in async mode is redundant",
          );
        }
        const notifyParams: Record<string, unknown> = {
          task_id: params.task_id,
          once: coerceBoolean(params.once, true),
        };
        if (waitFor.kind === "regex") notifyParams.regex = waitFor.source;
        else notifyParams.pattern = waitFor.value;
        const sessionId = resolveSessionId(extCtx);
        markExplicitControl(sessionId, params.task_id, false);
        let registered: Record<string, unknown>;
        try {
          registered = await callBashBridge(bridge, "bash_notify", notifyParams, extCtx);
        } catch (err) {
          unmarkExplicitControl(sessionId, params.task_id);
          throw err;
        }
        if (registered.success === false) {
          unmarkExplicitControl(sessionId, params.task_id);
          const message = String(registered.message ?? "bash_notify failed");
          throw new Error(`${String(registered.code ?? "invalid_request")}: ${message}`);
        }
        const watchDetails = { registered: true, watchId: registered.watch_id } as BashWatchDetails;
        return textResult(
          `Watch registered: ${registered.watch_id} on task ${params.task_id}\nA notification will fire when the pattern matches or the task exits.`,
          watchDetails,
        );
      }
      const syncWaitCap = bashCfg.watch_sync_max_ms;
      const role = watchCallerRole(extCtx);
      const effectiveWaitMs = workerForcedSync
        ? bashCfg.worker_wait_max_ms
        : resolveWatchTimeoutMs(
            coerceConfiguredWatchTimeout(params.timeout_ms, role, syncWaitCap),
            role,
            syncWaitCap,
            bashCfg.worker_wait_max_ms,
          );
      const data = await waitForBashStatus(
        ctx,
        bridge,
        extCtx,
        params.task_id,
        undefined,
        waitFor,
        true,
        effectiveWaitMs,
        role,
        signal,
      );
      // User-message abort: the sync wait was interrupted because the user
      // sent a message. Auto-register the equivalent async watch so the
      // notification still arrives, and return text explaining the conversion.
      if (data.waited?.reason === "user_message") {
        const convertedText = await convertToAsyncWatchOnAbort(
          bridge,
          extCtx,
          params.task_id,
          waitFor,
          coerceBoolean(params.once, true),
          data.waited.elapsed_ms,
          role,
        );
        return textResult(withKillDeadline(convertedText, data, role), {
          waited: data.waited,
        } as BashWatchDetails);
      }
      const text = await formatBashStatus(
        extCtx,
        params.task_id,
        data as unknown as BashStatusDetails,
        undefined,
        { role, capMs: syncWaitCap },
      );
      return textResult(withKillDeadline(text, data, role), {
        ...data,
        effectiveWaitMs,
      } as BashWatchDetails);
    },
  };
}

/**
 * When a sync bash_watch wait is aborted because the user sent a message,
 * auto-register the equivalent async watch so the notification still arrives.
 * If the original sync watch had a pattern, register an async watch with the
 * same pattern. If it had no pattern (exit-only), the auto-reminder system
 * already handles exit notifications, so just return the conversion message.
 */
async function convertToAsyncWatchOnAbort(
  bridge: AftProjectTransport,
  extCtx: ExtensionContext,
  taskId: string,
  waitFor: BashWaitPattern | undefined,
  once: boolean,
  elapsedMs: number,
  role: WatchCallerRole,
): Promise<string> {
  const interrupted = `Sync watch for task ${taskId} was interrupted because you sent a message after ${elapsedMs}ms of waiting. `;
  const reminderFallback = interruptedWatchTail(
    role,
    `The task is still running in the background. A completion reminder will be ` +
      `delivered automatically when the task exits.`,
  );
  // No pattern: the auto-reminder system already handles exit notifications
  // for background tasks, so no explicit watch registration is needed.
  if (!waitFor) {
    return (
      interrupted +
      interruptedWatchTail(
        role,
        `The task is still running in the background. A completion reminder will be ` +
          `delivered automatically when the task exits; don't poll bash_status.`,
      )
    );
  }
  // Register the equivalent async watch so the pattern/exit notification
  // still arrives. Reuse the same registration path as the explicit async mode.
  const notifyParams: Record<string, unknown> = {
    task_id: taskId,
    once,
  };
  if (waitFor.kind === "regex") notifyParams.regex = waitFor.source;
  else notifyParams.pattern = waitFor.value;
  const sessionId = resolveSessionId(extCtx);
  markExplicitControl(sessionId, taskId, false);
  try {
    const registered = await callBashBridge(bridge, "bash_notify", notifyParams, extCtx);
    if (registered.success === false) {
      unmarkExplicitControl(sessionId, taskId);
      return (
        interrupted +
        `Auto-registering an async watch failed (${String(registered.message ?? "unknown error")}). ` +
        reminderFallback
      );
    }
    return (
      interrupted +
      `The wait has been converted to an async watch (${registered.watch_id}). ` +
      interruptedWatchTail(
        role,
        `A notification will fire when the pattern matches or the task exits.`,
      )
    );
  } catch (err) {
    unmarkExplicitControl(sessionId, taskId);
    return (
      interrupted +
      `Auto-registering an async watch failed (${err instanceof Error ? err.message : String(err)}). ` +
      reminderFallback
    );
  }
}

export function createBashWriteTool(ctx: PluginContext) {
  return {
    name: "bash_write",
    label: "bash_write",
    description:
      // Suggest checking the task mode first only when bash_status is callable.
      `Write input bytes to a running PTY bash task. PTY-only${bashCompanionRegistered(ctx.config, "bash_status") ? '; check bash_status reports mode: "pty" first.' : "."} ` +
      'Input is either a string (verbatim bytes) or an array mixing strings and { key: "esc" | "enter" | "up" | "ctrl-c" | ... } objects ' +
      'for atomic text+key sequences such as [ "iHello", { key: "esc" }, ":wq", { key: "enter" } ]. ' +
      "Named keys cover enter/return/tab/space/backspace/esc/escape, arrows, home/end/page-up/page-down/delete/insert, f1..f12, and ctrl-a..ctrl-z. " +
      "Maximum 1 MiB per call (post-expansion).",
    promptSnippet: "Write keystrokes/input to a PTY bash task",
    parameters: BashWriteParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof BashWriteParams>,
      _signal: AbortSignal | undefined,
      _onUpdate: ((update: AgentToolResult<BashWriteDetails>) => void) | undefined,
      extCtx: ExtensionContext,
    ) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const data = await callBashBridge(
        bridge,
        "bash_write",
        { task_id: params.task_id, input: params.input },
        extCtx,
      );
      return textResult(
        JSON.stringify({ bytes_written: data.bytes_written }, null, 2),
        data as unknown as BashWriteDetails,
      );
    },
  };
}

export function createBashKillTool(ctx: PluginContext) {
  return {
    name: "bash_kill",
    label: "bash_kill",
    description:
      "Terminate a running background bash task spawned with bash({ background: true }).",
    promptSnippet: "Kill a background bash task by task_id",
    parameters: BashTaskParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof BashTaskParams>,
      _signal: AbortSignal | undefined,
      _onUpdate: ((update: AgentToolResult<BashKillDetails>) => void) | undefined,
      extCtx: ExtensionContext,
    ) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const data = await callBashBridge(bridge, "bash_kill", { task_id: params.task_id }, extCtx);
      if (data.success === false) {
        throw new Error((data.message as string | undefined) ?? "bash_kill failed");
      }
      const details = data as unknown as BashKillDetails;
      if (details.kill_signaled === true) {
        return bashKillResult(
          `Task ${params.task_id}: kill_signaled · reached ${details.kill_reached ?? 0} live descendants`,
          details,
        );
      }
      return bashKillResult(`Task ${params.task_id}: ${details.status}`, details);
    },
  };
}

function bashResult(
  output: string,
  details: Partial<BashDetails> & { streaming?: boolean },
): AgentToolResult<BashDetails> {
  return {
    content: [{ type: "text", text: output }],
    details: {
      exit_code: details.exit_code,
      duration_ms: details.duration_ms,
      truncated: details.truncated,
      output_path: details.output_path,
      task_id: details.task_id,
      bg_completions: details.bg_completions,
    } as BashDetails,
  };
}

function bashStatusResult(
  output: string,
  details: BashStatusDetails,
): AgentToolResult<BashStatusDetails> {
  return {
    content: [{ type: "text", text: output }],
    details,
  };
}

function bashKillResult(
  output: string,
  details: BashKillDetails,
): AgentToolResult<BashKillDetails> {
  return {
    content: [{ type: "text", text: output }],
    details,
  };
}

type BashWaitPattern = { kind: "substring"; value: string } | { kind: "regex"; source: string };
type OutputStream = "output" | "stderr";
type OutputCursor = { output: number; stderr: number };
type OutputScanChunk = { stream: OutputStream; text: string; baseOffset: number };
type OutputScanState = Record<OutputStream, { text: string; baseOffset: number }>;

async function bashStatusSnapshot(
  bridge: AftProjectTransport,
  extCtx: ExtensionContext,
  taskId: string,
  outputMode: string | undefined,
  options?: BridgeRequestOptions,
  cursor?: OutputCursor,
): Promise<Record<string, unknown>> {
  return await callBashBridge(
    bridge,
    "bash_status",
    {
      task_id: taskId,
      output_mode: outputMode,
      output_offset: cursor?.output,
      stderr_offset: cursor?.stderr,
    },
    extCtx,
    options,
  );
}

async function waitForBashStatus(
  ctx: PluginContext,
  bridge: AftProjectTransport,
  extCtx: ExtensionContext,
  taskId: string,
  outputMode: string | undefined,
  waitFor: BashWaitPattern | undefined,
  waitForExit: boolean,
  effectiveWaitMs: number | undefined,
  role: WatchCallerRole,
  abortSignal?: AbortSignal,
): Promise<Record<string, unknown> & { waited: BashStatusWaited }> {
  // The deadline and the reported elapsed time both come from a monotonic
  // clock, so a wall-clock step during the wait can neither end it early nor
  // inflate the time the reply says it waited. An undefined wait has no
  // deadline: it ends only on exit, a match, a new message, or an abort.
  const startedAt = watchClock.now();
  const deadline =
    effectiveWaitMs === undefined ? Number.POSITIVE_INFINITY : startedAt + effectiveWaitMs;
  const elapsedMs = () => Math.round(watchClock.now() - startedAt);
  // Sleep until the next poll, never past the deadline. The poll interval
  // grows with the time already waited (watchPollDelayMs).
  const pause = (pastDeadline = false) => {
    const delay = watchPollDelayMs(elapsedMs());
    return watchClock.sleep(
      pastDeadline ? delay : Math.min(delay, Math.max(0, deadline - watchClock.now())),
      abortSignal,
    );
  };
  const waited = (
    reason: BashStatusWaited["reason"],
    extra: Partial<BashStatusWaited> = {},
  ): BashStatusWaited => ({
    reason,
    elapsed_ms: elapsedMs(),
    ...(effectiveWaitMs === undefined ? {} : { limit_ms: effectiveWaitMs }),
    ...extra,
  });
  let spillCursor: OutputCursor = { output: 0, stderr: 0 };
  const scanState: OutputScanState = {
    output: { text: "", baseOffset: 0 },
    stderr: { text: "", baseOffset: 0 },
  };
  const bridgeOptions = {
    keepBridgeOnTimeout: true,
    transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS,
  };
  if (waitFor?.kind === "regex") {
    await validateWaitRegex(bridge, extCtx, waitFor);
  }

  // Pre-mark BEFORE first poll: ingestBgCompletions will suppress any push
  // frame that arrives while we're waiting, so no wake is ever scheduled for
  // this task. Mirrors the OpenCode fix; see bg-notifications.markTaskWaiting.
  const sessionId = resolveSessionId(extCtx);
  // Clear any stale abort flag from a previous turn so it doesn't insta-abort
  // this new wait.
  clearSyncWatchAbort(sessionId);
  if (waitForExit) markTaskWaiting(sessionId, taskId);
  let sawTerminal = false;
  let lastData: Record<string, unknown> | undefined;
  // Start of the current run of status polls that all timed out, if any.
  let busySince: number | undefined;
  try {
    while (true) {
      if (abortSignal?.aborted) {
        return withWaited(lastData ?? unavailableSnapshot(), waited("aborted"));
      }
      let data: Record<string, unknown>;
      try {
        data = await bashStatusSnapshot(
          bridge,
          extCtx,
          taskId,
          outputMode,
          bridgeOptions,
          waitFor ? spillCursor : undefined,
        );
      } catch (err) {
        // A single poll's transport timeout means the bridge is *busy*, not
        // that the task failed — the bridge is kept warm (keepBridgeOnTimeout).
        // Don't abort the whole watch (which surfaces a red failure every
        // poll); honor abort/deadline and otherwise retry. A genuine
        // non-timeout error still propagates.
        if (!isBridgeTransportTimeout(err)) throw err;
        busySince ??= watchClock.now();
        if (isSyncWatchAborted(sessionId)) {
          return withWaited(lastData ?? unavailableSnapshot(), waited("user_message"));
        }
        // A wait with no deadline still gives up on a bridge that has not
        // answered for WATCH_UNAVAILABLE_GIVE_UP_MS, instead of retrying forever.
        if (
          watchClock.now() >= deadline ||
          watchClock.now() - busySince >= WATCH_UNAVAILABLE_GIVE_UP_MS
        ) {
          return withWaited(lastData ?? unavailableSnapshot(), waited("unavailable"));
        }
        await pause();
        continue;
      }
      busySince = undefined;
      lastData = data;
      const terminal = isTerminalStatus(data.status);

      if (waitFor) {
        const scan = await readNewTaskOutput(data, spillCursor);
        if (scan) {
          spillCursor = scan.nextCursor;
          // Independent buffers prevent a stdout suffix and stderr prefix from
          // becoming a fabricated match. When both streams match in one poll,
          // readNewTaskOutput's stdout-first chunk order is the tie-breaker.
          for (const chunk of scan.chunks) {
            const state = scanState[chunk.stream];
            if (state.text.length === 0) state.baseOffset = chunk.baseOffset;
            state.text += chunk.text;
            if (waitFor.kind === "regex") {
              const trimmed = trimWaitScanBuffer(state.text, state.baseOffset, waitFor);
              state.text = trimmed.text;
              state.baseOffset = trimmed.baseOffset;
            }
            const match = await findWaitMatch(bridge, extCtx, state.text, waitFor);
            if (match) {
              if (waitForExit && terminal) {
                sawTerminal = true;
                consumeBgCompletion(sessionId, taskId);
                await markBgCompletionDelivered(
                  { ctx, directory: extCtx.cwd, sessionID: sessionId },
                  taskId,
                );
              }
              const matchStream: "stdout" | "stderr" | undefined =
                data.mode === "pty" ? undefined : chunk.stream === "output" ? "stdout" : "stderr";
              return withWaited(
                data,
                waited("matched", {
                  match: match.text,
                  match_offset: state.baseOffset + match.byteOffset,
                  match_stream: matchStream,
                }),
              );
            }
            if (waitFor.kind === "substring") {
              const trimmed = trimWaitScanBuffer(state.text, state.baseOffset, waitFor);
              state.text = trimmed.text;
              state.baseOffset = trimmed.baseOffset;
            }
          }
        }
      }

      if (terminal) {
        if (waitForExit) {
          sawTerminal = true;
          consumeBgCompletion(sessionId, taskId);
          await markBgCompletionDelivered(
            { ctx, directory: extCtx.cwd, sessionID: sessionId },
            taskId,
          );
        }
        return withWaited(data, waited("exited"));
      }

      // User-message abort: if the user sent a message while we were
      // blocking, convert this sync wait to an async watch so the agent's
      // turn ends promptly. The match/exit checks above win over abort.
      if (isSyncWatchAborted(sessionId)) {
        return withWaited(data, waited("user_message"));
      }

      const waitPastDeadline =
        role === "worker" &&
        watchClock.now() >= deadline &&
        taskKillDeadlineWithinHandoffMargin(data);
      if (watchClock.now() >= deadline && !waitPastDeadline) {
        return withWaited(data, waited("timeout"));
      }
      await pause(waitPastDeadline);
    }
  } finally {
    if (waitForExit && !sawTerminal) unmarkTaskWaiting(sessionId, taskId);
  }
}

async function readNewTaskOutput(
  data: Record<string, unknown>,
  cursor: OutputCursor,
): Promise<{ chunks: OutputScanChunk[]; nextCursor: OutputCursor } | undefined> {
  const stdoutBytes =
    typeof data.output_chunk_base64 === "string"
      ? Buffer.from(data.output_chunk_base64, "base64")
      : Buffer.alloc(0);
  const stderrBytes =
    typeof data.stderr_chunk_base64 === "string"
      ? Buffer.from(data.stderr_chunk_base64, "base64")
      : Buffer.alloc(0);
  if (stdoutBytes.length + stderrBytes.length === 0) return undefined;
  const chunks: OutputScanChunk[] = [];
  if (stdoutBytes.length > 0) {
    chunks.push({
      stream: "output",
      text: stdoutBytes.toString("utf8"),
      baseOffset: cursor.output,
    });
  }
  if (stderrBytes.length > 0) {
    chunks.push({
      stream: "stderr",
      text: stderrBytes.toString("utf8"),
      baseOffset: cursor.stderr,
    });
  }
  return {
    chunks,
    nextCursor: {
      output:
        typeof data.output_next_offset === "number"
          ? data.output_next_offset
          : cursor.output + stdoutBytes.length,
      stderr:
        typeof data.stderr_next_offset === "number"
          ? data.stderr_next_offset
          : cursor.stderr + stderrBytes.length,
    },
  };
}

function parseWaitPattern(value: unknown): BashWaitPattern | undefined {
  if (typeof value === "string") return { kind: "substring", value };
  if (isRegexWaitObject(value)) return { kind: "regex", source: value.regex };
  return undefined;
}

export function __parseWaitPatternForTests(value: unknown): BashWaitPattern | undefined {
  return parseWaitPattern(value);
}

function isRegexWaitObject(value: unknown): value is { regex: string } {
  return (
    typeof value === "object" &&
    value !== null &&
    "regex" in value &&
    typeof (value as { regex?: unknown }).regex === "string"
  );
}

type WaitMatch = { text: string; byteOffset: number };

async function validateWaitRegex(
  bridge: AftProjectTransport,
  extCtx: ExtensionContext,
  pattern: Extract<BashWaitPattern, { kind: "regex" }>,
): Promise<void> {
  await matchRegexWithBridge(bridge, extCtx, pattern.source, "");
}

async function findWaitMatch(
  bridge: AftProjectTransport,
  extCtx: ExtensionContext,
  text: string,
  pattern: BashWaitPattern,
): Promise<WaitMatch | undefined> {
  if (pattern.kind === "substring") {
    const index = text.indexOf(pattern.value);
    return index >= 0
      ? { text: pattern.value, byteOffset: Buffer.byteLength(text.slice(0, index), "utf8") }
      : undefined;
  }
  return await matchRegexWithBridge(bridge, extCtx, pattern.source, text);
}

async function matchRegexWithBridge(
  bridge: AftProjectTransport,
  extCtx: ExtensionContext,
  pattern: string,
  text: string,
): Promise<WaitMatch | undefined> {
  try {
    const result = await callBashBridge(bridge, "bash_regex_match", { pattern, text }, extCtx);
    if (result.matched !== true) return undefined;
    return {
      text: typeof result.match_text === "string" ? result.match_text : "",
      byteOffset: coerceMatchOffset(result.match_offset),
    };
  } catch (err) {
    if (err instanceof BridgeError && err.code === "invalid_regex") {
      throw new Error(`invalid_request: invalid_regex: ${err.message}`);
    }
    throw err;
  }
}

function coerceMatchOffset(value: unknown): number {
  const offset = typeof value === "number" ? value : Number(value ?? 0);
  return Number.isFinite(offset) && offset >= 0 ? offset : 0;
}

function trimWaitScanBuffer(
  text: string,
  baseOffset: number,
  pattern: BashWaitPattern,
): { text: string; baseOffset: number } {
  const keepFrom =
    pattern.kind === "substring"
      ? substringKeepStart(text, pattern.value)
      : regexKeepStart(text, REGEX_WAIT_SCAN_WINDOW_BYTES);
  if (keepFrom <= 0) return { text, baseOffset };

  return {
    text: text.slice(keepFrom),
    baseOffset: baseOffset + Buffer.byteLength(text.slice(0, keepFrom), "utf8"),
  };
}

function substringKeepStart(text: string, pattern: string): number {
  const keepChars = Math.max(0, pattern.length - 1);
  return text.length > keepChars ? text.length - keepChars : 0;
}

function regexKeepStart(text: string, maxBytes: number): number {
  if (Buffer.byteLength(text, "utf8") <= maxBytes) return 0;

  let low = 0;
  let high = text.length;
  while (low < high) {
    const mid = Math.floor((low + high) / 2);
    if (Buffer.byteLength(text.slice(mid), "utf8") > maxBytes) {
      low = mid + 1;
    } else {
      high = mid;
    }
  }
  return low;
}

export function __trimWaitScanBufferForTests(
  text: string,
  baseOffset: number,
  pattern: BashWaitPattern,
): { text: string; baseOffset: number } {
  return trimWaitScanBuffer(text, baseOffset, pattern);
}

/**
 * Every watch result also names the task's own kill deadline (or the limit
 * that killed it), kept apart from how long the watch waited.
 */
function withKillDeadline(
  text: string,
  data: Record<string, unknown>,
  role: WatchCallerRole,
): string {
  const deadline = taskKillDeadlineText(data, role);
  return deadline === "" ? text : `${text}\n${deadline}`;
}

function withWaited(
  data: Record<string, unknown>,
  waited: BashStatusWaited,
): Record<string, unknown> & { waited: BashStatusWaited } {
  return { ...data, waited };
}

function formatWaitSummary(
  taskId: string,
  waited: BashStatusWaited,
  details: BashStatusDetails,
  watchRole: WatchRoleContext,
): string {
  // Only a primary's watch is bounded by the configured cap.
  const waitedText = formatWatchWaited(
    waited.elapsed_ms,
    waited.limit_ms,
    watchRole.role === "primary" ? watchRole.capMs : undefined,
  );
  if (waited.reason === "matched") {
    const stream = waited.match_stream ? ` in ${waited.match_stream}` : "";
    return `${waitedText}; matched ${JSON.stringify(waited.match ?? "")}${stream} at offset ${waited.match_offset ?? 0}.`;
  }
  if (waited.reason === "timeout" && watchRole.role === "worker") {
    // A watch deadline is not a failure of the command, and a delegated
    // worker that reads it as one declares a failed result mid-run. Tell it
    // the command is still running, how long it has run and what it last
    // printed, and how to wait again or stop it.
    return `${waitedText}; timeout reached without match. ${workerWatchStillRunning({
      taskId,
      waitedMs: waited.elapsed_ms,
      ranMs: details.duration_ms,
      output: details.output_preview,
      taskIdArg: "task_id",
      timeoutParam: "timeout_ms",
    })}`;
  }
  if (waited.reason === "timeout") {
    // A watch deadline is not a failure of the command; the steer tells the
    // caller so.
    return `${waitedText}; timeout reached without match. ${watchTimeoutSteer()}`;
  }
  if (waited.reason === "unavailable") {
    return `${waitedText}; ${watchUnavailableSteer(watchRole.role)}`;
  }
  if (waited.reason === "aborted") {
    return `${waitedText}; the watch was cancelled. The task keeps running.`;
  }
  const exit = typeof details.exit_code === "number" ? `, exit ${details.exit_code}` : "";
  return `${waitedText}; task exited (${details.status}${exit}).`;
}

/** Role and resolved sync cap used to word a bash_watch timeout reply. */
type WatchRoleContext = { role: WatchCallerRole; capMs: number };

async function formatBashStatus(
  extCtx: ExtensionContext,
  taskId: string,
  details: BashStatusDetails,
  requestedOutputMode: string | undefined,
  watchRole: WatchRoleContext,
): Promise<string> {
  const exit = typeof details.exit_code === "number" ? ` (exit ${details.exit_code})` : "";
  const dur =
    typeof details.duration_ms === "number" ? ` ${Math.round(details.duration_ms / 1000)}s` : "";
  let text = `Task ${taskId}: ${details.status}${exit}${dur}`;
  if (details.live_descendants_summary) {
    text += ` · ${details.live_descendants_summary}`;
  }
  if (details.output_incomplete || (details.status === "failed" && details.status_reason)) {
    text += `\n[${details.status_reason || "PTY output may be incomplete"}]`;
  }
  if (details.waited)
    text += `
${formatWaitSummary(taskId, details.waited, details, watchRole)}`;
  if (details.mode === "pty") {
    // PTY output is rendered from the raw terminal spill file; never feed it
    // through the piped-output compression/line renderer.
    text += await formatPtyStatus(extCtx, taskId, details, requestedOutputMode);
  } else {
    if (isTerminalStatus(details.status) && details.output_preview) {
      text += `
${details.output_preview}`;
    }
    // A worker whose watch returned while the task still runs has already
    // been told to watch again; adding "don't poll" would contradict it.
    const workerWatchReturned = watchRole.role === "worker" && details.waited !== undefined;
    if (!isTerminalStatus(details.status) && !workerWatchReturned) {
      text += `
${runningTaskStatusHint(watchRole.role)}`;
    }
  }
  return text;
}

async function formatPtyStatus(
  _extCtx: ExtensionContext,
  taskId: string,
  details: BashStatusDetails,
  requestedOutputMode: string | undefined,
): Promise<string> {
  const outputMode = requestedOutputMode ?? "screen";
  const raw = typeof details.pty_raw === "string" ? details.pty_raw : "";
  let suffix = "";
  if (outputMode === "raw") {
    suffix = raw.length > 0 ? `\n${raw}` : "";
  } else if (outputMode === "both") {
    suffix = `\n${JSON.stringify({ screen: details.pty_screen ?? "", raw }, null, 2)}`;
  } else {
    suffix = details.pty_screen ? `\n${details.pty_screen}` : "";
  }
  if (!isTerminalStatus(details.status)) {
    suffix += `\nPTY task is still running. Use bash_status({ task_id: "${taskId}", output_mode: "screen" }) to inspect, bash_write({ task_id: "${taskId}", input: "..." }) to send keystrokes.`;
  }
  return suffix;
}

function renderBashCall(
  command: string | undefined,
  description: string | undefined,
  theme: Theme,
  context: RenderContextLike,
): Text {
  const text = reuseText(context.lastComponent);
  // While arguments are still streaming, keep the "..." placeholder the
  // renderer has always shown rather than a bare title.
  const display = description ?? (command ? shortenCommand(command) : "...");
  text.setText(`${theme.fg("toolTitle", theme.bold("bash"))} ${theme.fg("accent", display)}`);
  return text;
}

function renderBashResult(
  result: AgentToolResult<BashDetails>,
  theme: Theme,
  context: RenderContextLike,
  options: RenderResultOptionsLike = { expanded: true },
): import("@earendil-works/pi-tui").Component {
  // Errors: red text with error details
  if (context.isError) {
    const errorText = result.content
      .filter((c) => c.type === "text")
      .map((c) => (c as { text?: string }).text ?? "")
      .join("\n")
      .trim();
    const text = reuseText(context.lastComponent);
    text.setText(`\n${theme.fg("error", errorText || "bash failed")}`);
    return text;
  }

  const details = result.details;
  const exitCode = details?.exit_code;
  const bgCompletions = details?.bg_completions ?? [];

  // Build result display
  const container = reuseContainer(context.lastComponent);
  container.clear();
  container.addChild(new Spacer(1));

  // Output preview is already capped by Rust's coordinated bash-output policy.
  const rawOutput = result.content
    .filter((c) => c.type === "text")
    .map((c) => (c as { text?: string }).text ?? "")
    .join("\n")
    .trim();
  if (rawOutput) {
    container.addChild(new Text(rawOutput, 1, 0));
    container.addChild(new Spacer(1));
  }

  // Exit code indicator
  if (exitCode !== undefined) {
    const exitColor = exitCode === 0 ? "success" : "error";
    const exitText = theme.fg(exitColor, `exit ${exitCode}`);
    container.addChild(new Text(exitText, 1, 0));
  }

  // Background completions notification (from Track D metadata)
  if (bgCompletions.length > 0) {
    container.addChild(new Spacer(1));
    for (const bg of bgCompletions) {
      const cmdPreview = bg.command ? bg.command.slice(0, 60) : "unknown command";
      const suffix = (bg.command?.length ?? 0) > 60 ? "..." : "";
      const exitInfo = bg.exit_code !== undefined ? `exit ${bg.exit_code}` : bg.status;
      const statusColor = bg.status === "completed" && bg.exit_code === 0 ? "success" : "warning";
      const line = theme.fg(
        statusColor,
        `Background task ${bg.task_id} completed (${exitInfo}): ${cmdPreview}${suffix}`,
      );
      container.addChild(new Text(line, 1, 0));
    }
  }

  // Duration info (muted)
  if (details?.duration_ms !== undefined) {
    container.addChild(new Spacer(1));
    const durationText = theme.fg("muted", `${details.duration_ms}ms`);
    container.addChild(new Text(durationText, 1, 0));
  }

  // Truncation notice
  if (details?.truncated) {
    container.addChild(new Spacer(1));
    const truncText = theme.fg("warning", "(output truncated)");
    container.addChild(new Text(truncText, 1, 0));
  }

  const firstOutputLine = rawOutput.split(/\r?\n/, 1)[0] || "(no output)";
  const exitSummary = exitCode === undefined ? "completed" : `exit ${exitCode}`;
  return collapsibleResult({
    summary: `${exitSummary}: ${firstOutputLine}`,
    full: container,
    expanded: options.expanded,
    context,
  });
}

function shortenCommand(command: string): string {
  // Truncate long commands for UI display
  if (command.length <= 60) return command;
  return `${command.slice(0, 57)}...`;
}
