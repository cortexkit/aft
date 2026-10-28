import {
  BASH_HOST_FALLBACK_REFUSAL,
  BASH_RETURN_PATHS_DESCRIPTION,
  BASH_RUNON_DESCRIPTION,
  BASH_RUNON_GUIDANCE,
  type BashPermissionAsk,
  type BridgeRequestOptions,
  bashHostFallbackAskPattern,
  classifyBashHostFallbackError,
  coerceBoolean,
  escalationAskText,
  formatBashOutputPages,
  groupBashPermissionAsks,
  isLiveBackgroundTaskReply,
  LONGEST_TIMER_DELAY_MS,
  maybeAppendGrepSearchHint,
  refuseAftModuleOptionWithoutFallback,
  runBashHostFallback,
  runningTaskStatusHint,
  sleep,
  type WatchCallerRole,
  WORKER_WAIT_LIMIT_PHRASE,
  withBashPermissionLoop,
  workerBackgroundTaskNote,
} from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";

import { trackBgTask } from "../bg-notifications.js";
import { remoteRunsOffered, resolveBashConfig, toolEnabled } from "../config.js";
import { flushLog, sessionLog } from "../logger.js";
import { normalizeToolArgSchemas } from "../normalize-schemas.js";
import { resolveIsSubagent, resolveSubagentStatus } from "../shared/subagent-detect.js";
import type { PluginContext } from "../types.js";
import { trackV2BackgroundTask, v2StopEpoch } from "../v2-session-lifecycle.js";
import { callBashBridge, coerceOptionalInt, optionalInt, projectRootFor } from "./_shared.js";
import { metadataUpdater } from "./metadata.js";
import { withHostPermissionGrants } from "./outside-permissions.js";
import { runAsk } from "./permissions.js";

const z = tool.schema;
const METADATA_PREVIEW_LIMIT = 30 * 1024;
export const BASH_SECRET_HOST_FALLBACK_REFUSAL =
  "Secret placeholder commands need AFT's shell; OpenCode's built-in shell cannot run them.";
// Default hard timeout of 30 minutes when the caller omits a timeout. This
// sizes the bridge transport timeout for bash calls where the server blocks
// until the command completes or is killed.
const DEFAULT_HARD_TIMEOUT_MS = 30 * 60 * 1000;
// The margin gives Rust time to promote or finalize the task and deliver the
// final response after the server's foreground wait window or hard kill timeout.
const BASH_TRANSPORT_MARGIN_MS = 10_000;
// Mirrors Rust's REMOTE_INPUT_CAPTURE_WAIT: runon launch replies may wait for capture.
const REMOTE_INPUT_CAPTURE_WAIT_MS = 60_000;
const ABORT_REGISTRATION_WAIT_MS = 5_000;
const ABORT_REGISTRATION_POLL_MS = 25;
const ABORT_ATTEMPT_TIMEOUT_MS = 1_000;
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

function orchestratedTransportTimeoutMs(
  blockToCompletion: boolean,
  wait: boolean,
  effectiveTimeout: number | undefined,
  foregroundWaitMs: number,
  workerWaitMaxMs?: number,
  runon = false,
): number {
  const blocking = blockToCompletion || wait;
  let waitBudget = blocking ? (effectiveTimeout ?? DEFAULT_HARD_TIMEOUT_MS) : foregroundWaitMs;
  // AFT returns every blocking call at bash.worker_wait_max_ms, even if
  // renewing the default kill deadline lets the command run longer.
  if (blocking && workerWaitMaxMs !== undefined) {
    waitBudget = Math.min(effectiveTimeout ?? workerWaitMaxMs, workerWaitMaxMs);
  }
  if (runon) {
    // Capture shares the command timeout, but can outlast the foreground window.
    const captureBudget = Math.min(
      effectiveTimeout ?? REMOTE_INPUT_CAPTURE_WAIT_MS,
      REMOTE_INPUT_CAPTURE_WAIT_MS,
      blocking ? (workerWaitMaxMs ?? REMOTE_INPUT_CAPTURE_WAIT_MS) : REMOTE_INPUT_CAPTURE_WAIT_MS,
    );
    waitBudget = Math.max(waitBudget, captureBudget);
  }
  // A configured limit can exceed what a JavaScript timer accepts.
  return Math.min(waitBudget + BASH_TRANSPORT_MARGIN_MS, LONGEST_TIMER_DELAY_MS);
}

type ForegroundAbortOutcome = "killed" | "call_settled" | "timed_out";

async function abortForegroundWhenRegistered(
  ctx: PluginContext,
  runtime: ToolContext,
  disposed: () => boolean,
): Promise<void> {
  const startedAt = Date.now();
  const deadline = startedAt + ABORT_REGISTRATION_WAIT_MS;
  // One entry per bash_abort_inflight attempt, in order: `killed=<n>` for an
  // answer and `error=<message>` for a failed send. Only the first few are
  // kept so a five-second registration wait cannot flood the log.
  const results: string[] = [];
  let attempts = 0;
  let lastResponse: Record<string, unknown> | undefined;
  let lastError: string | undefined;
  let outcome: ForegroundAbortOutcome = "timed_out";

  while (Date.now() < deadline) {
    if (disposed()) {
      outcome = "call_settled";
      break;
    }
    attempts += 1;
    try {
      lastResponse = await callBashBridge(
        ctx,
        runtime,
        "bash_abort_inflight",
        {},
        {
          transportTimeoutMs: Math.max(
            1,
            Math.min(ABORT_ATTEMPT_TIMEOUT_MS, deadline - Date.now()),
          ),
        },
      );
      lastError = undefined;
      recordAbortAttempt(results, `killed=${String(lastResponse.killed)}`);
      if (typeof lastResponse.killed === "number" && lastResponse.killed > 0) {
        outcome = "killed";
        break;
      }
    } catch (error) {
      lastError = error instanceof Error ? error.message : String(error);
      recordAbortAttempt(results, `error=${lastError}`);
    }

    if (disposed()) {
      outcome = "call_settled";
      break;
    }
    if (Date.now() >= deadline) break;
    await sleep(Math.min(ABORT_REGISTRATION_POLL_MS, deadline - Date.now()));
  }

  // Every outcome is logged, not only the timeout. An interrupted call whose
  // task row later lacks `call_aborted` can then be read back as "the abort
  // never ran", "it ran and Rust found nothing to kill", or "the call ended on
  // its own first". The line is flushed at once because a host often shuts
  // down right after an interruption, before a buffered line would be written.
  sessionLog(runtime.sessionID, `[bash] foreground abort ${outcome}`, {
    attempts,
    elapsed_ms: Date.now() - startedAt,
    results,
    ...(results.length < attempts ? { results_omitted: attempts - results.length } : {}),
    last_response: lastResponse,
    last_error: lastError,
  });
  flushLog();
}

const ABORT_ATTEMPT_RESULTS_KEPT = 20;

function recordAbortAttempt(results: string[], result: string): void {
  if (results.length < ABORT_ATTEMPT_RESULTS_KEPT) results.push(result);
}

function listenForForegroundAbort(
  ctx: PluginContext,
  runtime: ToolContext,
  enabled: boolean,
): () => void {
  if (!enabled) return () => {};

  let fired = false;
  let disposed = false;
  const onAbort = () => {
    if (fired) return;
    fired = true;
    // The host can interrupt this tool before Rust has registered the foreground
    // task. Wait on the abort result instead of losing that early interruption.
    // Pi exposes a different abort surface, so this parity hook is OpenCode-only.
    void abortForegroundWhenRegistered(ctx, runtime, () => disposed);
  };

  runtime.abort.addEventListener("abort", onAbort, { once: true });
  if (runtime.abort.aborted) onAbort();
  return () => {
    disposed = true;
    runtime.abort.removeEventListener("abort", onAbort);
  };
}

/**
 * Agent-facing wording follows the project-resolved setting so the wait behavior
 * and its per-message escape hatch are explicit to the agent.
 */
function userMessageDetachDescription(detachOnUserMessage: boolean): string {
  return detachOnUserMessage
    ? "Any new message detaches this wait. Set `bash.detach_on_user_message: false` to keep it blocking; even then, a message containing the literal `&detach` forces detachment, and the token is stripped before delivery and the rest of the message is preserved; a token-only message becomes `(requested background detach)`."
    : "Because `bash.detach_on_user_message` is false, a new message leaves this wait blocking; include the literal `&detach` anywhere to force detachment, and the token is stripped before delivery and the rest of the message is preserved; a token-only message becomes `(requested background detach)`.";
}

/**
 * Whether a bash companion tool (`bash_status`, `bash_watch`, `bash_write`,
 * `bash_kill`) is registered for this configuration: it needs
 * `bash.background` and must not be in `disabled_tools`. Descriptions use this
 * to name a companion only when the model can call it.
 */
export function bashCompanionRegistered(
  config: PluginContext["config"],
  companion: "bash_status" | "bash_watch" | "bash_write" | "bash_kill",
): boolean {
  return resolveBashConfig(config).background && toolEnabled(config, companion);
}

/**
 * How the description tells the agent to wait on a background task. Hosts that
 * register `bash_watch` steer short waits to it; the subc module catalog has no
 * `bash_watch`, so its variant names only the tools a catalog consumer can call.
 * `bash_status` is named only when it is registered.
 */
function backgroundWaitDescription(
  watchToolRegistered: boolean,
  statusRegistered: boolean,
  role: WatchCallerRole = "primary",
): string {
  if (watchToolRegistered && role === "worker") {
    // A delegated worker is never woken by a completion reminder, so it is
    // told only how to wait; the primary wording below offers ending the turn.
    const noPolling = statusRegistered ? "; never loop bash_status to wait" : "";
    return `then wait on it with bash_watch before you report a result: a background task never wakes you, and a watch without a timeout waits up to ${WORKER_WAIT_LIMIT_PHRASE}, then reports it is still running; watch again to keep waiting. Never background a command and immediately bash_watch it (that wastes a turn for what foreground returns in one)${noPolling}.`;
  }
  if (watchToolRegistered) {
    const noPolling = statusRegistered ? ", and never loop bash_status to wait" : "";
    return `then bash_watch handles only a short remaining wait (in a main session a watch defaults to 30s, max bash.watch_sync_max_ms, 120s by default; in a delegated session a watch without a timeout waits up to the worker wait limit, bash.worker_wait_max_ms, 30 minutes by default, then reports it is still running; watch again to keep waiting); for anything longer end the turn and let the completion reminder wake you, or use bash({wait:true}) when the result is needed before anything else — never background a command and immediately bash_watch it (that wastes a turn for what foreground returns in one)${noPolling}.`;
  }
  return statusRegistered
    ? "the task keeps running after the call returns, a completion reminder arrives when it exits, and bash_status reports its state and output. Use bash({wait:true}) instead when the result is needed before anything else."
    : "the task keeps running after the call returns and a completion reminder arrives when it exits. Use bash({wait:true}) instead when the result is needed before anything else.";
}

/** How a PTY session is driven, naming only the companions that exist. */
function ptyDriveClause(statusRegistered: boolean, writeRegistered: boolean): string {
  if (statusRegistered && writeRegistered) {
    return ', and is driven with bash_status({ outputMode: "screen" }) plus bash_write';
  }
  if (statusRegistered)
    return ', and its screen is read with bash_status({ outputMode: "screen" })';
  if (writeRegistered) return ", and is driven with bash_write";
  return "";
}

/**
 * Which neighbouring tools the bash description may name. Each defaults to
 * registered; `watchToolRegistered`, `aftSearchRegistered` and `zoomEnabled`
 * are separate positional arguments for historical reasons.
 */
export interface BashDescriptionSurface {
  outline?: boolean;
  status?: boolean;
  write?: boolean;
  /**
   * Whose surface this describes. Defaults to `primary`, the wording every
   * plugin registration uses (it covers both roles). The subc module
   * catalog's `worker` preset asks for `worker`, which never offers a
   * completion reminder or ending the turn.
   */
  role?: WatchCallerRole;
  /** Add remote-run guidance only where the tool also offers its parameter. */
  remoteRuns?: boolean;
}

export function bashToolDescription(
  aftSearchRegistered: boolean,
  compressionOn: boolean,
  backgroundOn: boolean,
  detachOnUserMessage = true,
  zoomEnabled = true,
  watchToolRegistered = true,
  surface: BashDescriptionSurface = {},
): string {
  const outline = surface.outline !== false;
  const status = surface.status !== false;
  const write = surface.write !== false;
  const role = surface.role ?? "primary";
  const autoPromote =
    role === "worker"
      ? "keep it off otherwise so a long command moves to the background while you work"
      : "keep it off otherwise so auto-promote can remind you while you work";
  const steerTools = [
    aftSearchRegistered ? "aft_search (concepts, identifiers, regex, literals)" : "the grep tool",
    "read",
    ...(outline ? ["aft_outline"] : []),
  ].join(", ");
  const searchSteer = `use ${steerTools}${zoomEnabled ? ", or aft_zoom" : ""} instead`;
  const compression = compressionOn
    ? " Output is compressed by default; pass compressed: false for raw output. Piped commands run verbatim and show the pipeline's output; for AFT's test/build summary, run the runner without | head, | tail, or | grep. Pipeline-failure notes cover single top-level pipelines only; multi-statement commands (`a; b | c; d`) are not instrumented, so masked failures inside them still need explicit exit-code checks."
    : "";
  const tasks = backgroundOn
    ? ` Commands run in the foreground and return inline; wait: true blocks until a long command finishes instead of auto-promoting (it blocks up to bash.worker_wait_max_ms, 30 minutes by default, then reports the command is still running; watch again to keep waiting); ${userMessageDetachDescription(detachOnUserMessage)} Use it when you need the result before doing anything else; ${autoPromote}. Use background: true yourself ONLY when you have other useful work to do while it runs; ${backgroundWaitDescription(watchToolRegistered, status, role)} Do not change or delete files a background command uses until it finishes. A \`nohup … &\` launch still holds the call if the child keeps stdout/stderr; redirect both or use background:true. pty: true runs interactive programs (REPLs, TUIs), implies background${ptyDriveClause(status, write)}.`
    : " Commands run in the foreground to completion; timeout is the hard kill cap (default 30 minutes).";
  const outputRules = backgroundOn
    ? " Finished output expires after the task is 24 hours old and its completion has been delivered; under project-root restrictions, only the starting session can read output outside the project—copy cited lines into your report."
    : "";
  const timeoutRules =
    " `timeout` starts after spawn and kills the Unix process group (exit 124; Windows uses taskkill /T /F); processes that leave the group survive.";
  const remoteRuns = surface.remoteRuns ? ` ${BASH_RUNON_GUIDANCE}` : "";
  return `Execute shell commands.${compression}${tasks}${outputRules}${timeoutRules}${remoteRuns}

DO NOT use bash for code search or code exploration. If you are about to run grep, rg, sed, awk, find, or cat through bash to locate or read code: STOP — ${searchSteer}. When a list is cut, the reply ends with \`shown N of M <unit> (<reason>) · narrow: <knobs>\`; absence of that line means the list is complete.`;
}

type BridgeCaller = typeof callBashBridge;

async function withPermissionLoop(
  ctx: PluginContext,
  runtime: ToolContext,
  params: Record<string, unknown>,
  bridgeCall: BridgeCaller,
  options?: BridgeRequestOptions,
): ReturnType<BridgeCaller> {
  return withBashPermissionLoop({
    request: (granted) =>
      bridgeCall(ctx, runtime, "bash", withHostPermissionGrants(params, granted), options),
    approve: async (asks: readonly BashPermissionAsk[]) => {
      for (const ask of groupBashPermissionAsks(asks)) {
        const permission = ask.kind === "external_directory" ? "external_directory" : "bash";
        const escalation = ask.kind === "escalation";
        await runAsk(
          runtime.ask({
            permission,
            patterns: escalation ? [escalationAskText(ask)] : (ask.patterns ?? []),
            always: escalation ? [] : (ask.always ?? []),
            metadata: escalation
              ? {
                  command: ask.command ?? "",
                  cwd: ask.cwd ?? "",
                  grant_id: ask.grant_id ?? "",
                  unsandboxed: true,
                }
              : ask.reason
                ? { reason: ask.reason }
                : {},
          }),
        );
      }
    },
  });
}

/** The PTY argument's sentence on driving a session, naming only registered companions. */
function ptyDriveParamSentence(statusRegistered: boolean, writeRegistered: boolean): string {
  const writeInput =
    'its input accepts either a string OR an array like [ "iHello", { key: "esc" }, ":wq", { key: "enter" } ] for atomic text+key sequences.';
  if (statusRegistered && writeRegistered) {
    return ` Inspect with bash_status({ taskId, outputMode: "screen" }) and drive interactively with bash_write — ${writeInput}`;
  }
  if (statusRegistered) return ' Inspect with bash_status({ taskId, outputMode: "screen" }).';
  if (writeRegistered) return ` Drive interactively with bash_write — ${writeInput}`;
  return "";
}

/**
 * Whether the native bash sandbox is on for this configuration. The `sandbox`
 * argument only asks to leave that sandbox for one command, so it is offered
 * only when there is a sandbox to leave.
 */
export function nativeSandboxEnabled(config: PluginContext["config"]): boolean {
  return config.sandbox?.profile?.head !== undefined && config.sandbox.profile.head !== "off";
}

/**
 * The `timeout` argument's description. The primary wording is what every
 * plugin registration shows; the subc module catalog's `worker` preset uses
 * the worker wording, because a worker's promoted command never sends it a
 * completion reminder.
 */
export function bashTimeoutDescription(
  backgroundOn: boolean,
  role: WatchCallerRole = "primary",
): string {
  if (!backgroundOn) {
    return "Hard kill cap in milliseconds (positive integer). When omitted, the foreground command can run up to 30 minutes and returns inline when it finishes.";
  }
  const promoted =
    role === "worker"
      ? "moves to the background as a task that won't wake you, so wait on it with bash_watch; wait:true disables promotion and remains inline until completion, the timeout, or the worker wait limit"
      : "is promoted to background and gets a completion reminder when it exits; wait:true disables promotion and remains inline until completion, timeout, or bash.worker_wait_max_ms";
  return `Hard kill cap in milliseconds (positive integer). In the default foreground mode when wait is false, a command that exceeds the configured wait window ${promoted}. A background task with no timeout is killed after 30 minutes; pass a longer timeout for long jobs.`;
}

export function createBashTool(
  ctx: PluginContext,
  aftSearchRegisteredOverride?: boolean,
): ToolDefinition {
  const initialBashCfg = resolveBashConfig(ctx.config);
  // Companion names appear in argument text only when the model can call them.
  const statusRegistered = bashCompanionRegistered(ctx.config, "bash_status");
  const writeRegistered = bashCompanionRegistered(ctx.config, "bash_write");
  const taskControls = [
    ...(statusRegistered ? ["bash_status"] : []),
    ...(bashCompanionRegistered(ctx.config, "bash_kill") ? ["bash_kill"] : []),
  ];
  // Each optional argument exists only while the feature it controls is on, so
  // a model is never offered a knob that does nothing: `wait`, `background` and
  // the PTY arguments need `bash.background`, `compressed` needs
  // `bash.compress`, and `sandbox` needs `sandbox.profile.head`. The set is fixed
  // when the tool is built; a stale call that still sends a removed argument is
  // ignored in `execute` rather than rejected.
  const waitArg = initialBashCfg.background
    ? {
        wait: z
          .boolean()
          .optional()
          .describe(
            `When true, run in the foreground without auto-promoting and wait until the command finishes or reaches its timeout (at most bash.worker_wait_max_ms, 30 minutes by default; the command then keeps running in the background and the reply says how to keep waiting); ${userMessageDetachDescription(initialBashCfg.detach_on_user_message)} Use only when you know the result is required before doing anything else.`,
          ),
      }
    : {};
  const sandboxArg = nativeSandboxEnabled(ctx.config)
    ? {
        sandbox: z
          .literal("host")
          .optional()
          .describe(
            "Request one-command approval to run unsandboxed on the host; use only when native sandboxing blocks required work, and note that it is a no-op when sandboxing is disabled.",
          ),
      }
    : {};
  const compressedArg = initialBashCfg.compress
    ? {
        compressed: z
          .boolean()
          .optional()
          .describe(
            "When true or omitted, return compressed output with noisy terminal control sequences reduced. Set to false for raw output.",
          ),
      }
    : {};
  const backgroundFlagArg = initialBashCfg.background
    ? {
        background: z
          .boolean()
          .optional()
          .describe(
            `When true, spawn the command in the background and return a taskId${taskControls.length > 0 ? ` for ${taskControls.join("/")}` : ""} instead of waiting for completion. Defaults to false. A background task with no timeout is killed after 30 minutes; pass a longer timeout for long jobs.`,
          ),
      }
    : {};
  const ptyArgs = initialBashCfg.background
    ? {
        pty: z
          .boolean()
          .optional()
          .describe(
            `When true, spawn the command in a real PTY for interactive programs (python/node/bash REPLs, vim). Implies background: true automatically.${initialBashCfg.subagent_background ? "" : " Unavailable in subagent sessions because bash.subagent_background is false."}${ptyDriveParamSentence(statusRegistered, writeRegistered)}`,
          ),
        ptyRows: optionalInt(1, 60).describe(
          "PTY terminal height in rows — ignored when pty is false. Defaults to 24 when pty: true. Minimum 1, maximum 60.",
        ),
        ptyCols: optionalInt(1, 140).describe(
          "PTY terminal width in columns — ignored when pty is false. Defaults to 80 when pty: true. Minimum 1, maximum 140.",
        ),
      }
    : {};
  // `runon` exists only in subc mode with remote runs enabled in the user
  // config (and not turned off by the project), decided once here; runner
  // health never adds or removes it.
  const remoteRuns = () => remoteRunsOffered(ctx.config);
  const runonArg = {
    runon: z.string().optional().describe(BASH_RUNON_DESCRIPTION),
    return_paths: z.array(z.string()).max(64).optional().describe(BASH_RETURN_PATHS_DESCRIPTION),
  };
  const args = {
    command: z
      .string()
      .describe("Shell command to execute. Supports pipes, redirection, and normal shell syntax."),
    timeout: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
      bashTimeoutDescription(initialBashCfg.background),
    ),
    workdir: z
      .string()
      .optional()
      .describe(
        "Working directory for command execution. Relative paths resolve through the bridge; defaults to the current tool context/project root when omitted.",
      ),
    description: z
      .string()
      .optional()
      .describe(
        "Short 5-10 word human-readable summary shown in OpenCode UI metadata instead of raw shell syntax.",
      ),
    ...waitArg,
    ...sandboxArg,
    ...backgroundFlagArg,
    ...compressedArg,
    ...ptyArgs,
  };

  // This state is deliberately local to one registered tool. Every command still
  // probes the module first; it only records whether the previous command used the
  // host path so the first successful module call clears fallback mode immediately.
  let hostFallbackActive = false;
  let surfaceDescription: string | undefined;

  return {
    get description() {
      const baseDescription =
        surfaceDescription ??
        bashToolDescription(
          false,
          initialBashCfg.compress,
          initialBashCfg.background,
          true,
          toolEnabled(ctx.config, "aft_zoom"),
          bashCompanionRegistered(ctx.config, "bash_watch"),
          {
            outline: toolEnabled(ctx.config, "aft_outline"),
            status: statusRegistered,
            write: writeRegistered,
            remoteRuns: false,
          },
        );
      return remoteRuns() ? `${baseDescription}\n\n${BASH_RUNON_GUIDANCE}` : baseDescription;
    },
    set description(value: string) {
      // Registration narrows companion wording after disabled tools are known.
      // Keep that wording, but derive remote guidance from the live gate.
      surfaceDescription = value
        .replaceAll(` ${BASH_RUNON_GUIDANCE}`, "")
        .replaceAll(`\n\n${BASH_RUNON_GUIDANCE}`, "");
    },
    get args() {
      return normalizeToolArgSchemas({
        args: { ...args, ...(remoteRuns() ? runonArg : {}) } as ToolDefinition["args"],
      }).args;
    },
    execute: async (args, context) => {
      const bashCfg = resolveBashConfig(ctx.config);
      const startedEpoch = v2StopEpoch(ctx.client, context.sessionID);
      const ctxAftSearchRegistered =
        (ctx as { aftSearchRegistered?: boolean }).aftSearchRegistered === true;
      const aftSearchRegistered = aftSearchRegisteredOverride ?? ctxAftSearchRegistered;
      let accumulatedOutput = "";
      const description = args.description as string | undefined;
      const metadata = metadataUpdater(context);
      const rawCommand = args.command as string;
      const command = rawCommand;
      const cwd = (args.workdir as string | undefined) ?? context.directory;

      // Detect whether the calling session is a subagent (has a non-empty parentID).
      const subagentStatus = await resolveSubagentStatus(
        ctx.client,
        context.sessionID,
        context.directory,
      );
      const isSubagent = subagentStatus === true;
      const backgroundDisabled = !bashCfg.background;
      // With background off, `wait` is not in the schema and every command
      // already runs to completion. A stale `wait: true` is ignored rather than
      // forwarded: the engine would otherwise make the call detachable on the
      // next user message, which would move it into a background task.
      const requestedWait = !backgroundDisabled && coerceBoolean(args.wait);
      const rawRequestedPty = coerceBoolean(args.pty);
      const rawRequestedBackground = coerceBoolean(args.background);
      // The contradiction checks below only fire while background is on:
      // requestedWait is false otherwise, so stale arguments never error.
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
      // pty:true silently implies background:true (Rust bash.rs handles the
      // auto-promote). Agents don't need to set both flags. When background is
      // disabled, those args are omitted from the schema and defensively ignored.
      const requestedBackground = !backgroundDisabled && (rawRequestedBackground || requestedPty);
      // ptyRows/ptyCols are silently ignored when pty is false so agents
      // that defensively pass them on normal bash calls don't get stuck in
      // a retry loop. pty: true silently implies background: true (Rust
      // bash.rs handles the auto-promote); no explicit check needed.
      const allowSubagentBg = bashCfg.subagent_background;
      // Unknown roles follow the worker restriction until the host can classify them.
      const subagentForcedForeground = subagentStatus !== false && !allowSubagentBg;
      if (requestedPty && subagentForcedForeground) {
        // A PTY session only exists as a background task, which
        // `bash.subagent_background: false` rules out for subagents; running it
        // in the foreground would just sit on the interactive program until the
        // hard timeout. With subagent background tasks allowed (the default), a
        // subagent can drive a PTY through bash_status and bash_write like a
        // primary session. Same rule and wording as the Pi plugin.
        throw new Error(
          "pty:true is unavailable in this subagent session because bash.subagent_background is false; run the command without pty.",
        );
      }
      const blockToCompletion = subagentForcedForeground || backgroundDisabled || requestedWait;
      const effectiveBackground = blockToCompletion ? false : requestedBackground;

      // `timeout` is the command's hard kill cap in every mode, forwarded
      // unchanged. On the default foreground path a cap shorter than the
      // foreground wait window kills the command before it would be promoted,
      // and the call answers with the timed-out result. Omitting it lets the
      // engine apply its 30-minute default.
      const rawTimeout = coerceOptionalInt(args.timeout, "timeout", 1, Number.MAX_SAFE_INTEGER);
      // AFT hands every blocking call back at bash.worker_wait_max_ms with
      // the command still running. The bridge's reply timeout must allow
      // that wait so it does not lose the handoff reply.
      const workerWaitMaxMs = bashCfg.worker_wait_max_ms;
      const ptyRows = coerceOptionalInt(args.ptyRows, "ptyRows", 1, 60);
      const ptyCols = coerceOptionalInt(args.ptyCols, "ptyCols", 1, 140);
      const compressed = coerceBoolean(args.compressed, true);
      // Forwarded whenever present, even if this surface does not offer it:
      // the engine refuses a remote run it cannot make by name, and a stale
      // `runon` must never quietly become a local run.
      const runon = args.runon;
      // Forwarded like `runon`: the engine refuses it without `runon`, and
      // a host fallback must not run the command and drop the request.
      const returnPaths = args.return_paths;
      if (runon !== undefined && !remoteRunsOffered(ctx.config)) {
        throw new Error(
          "runon refused: remote execution or bash.runon_enabled is unavailable in this session",
        );
      }
      const foregroundWaitMs = resolveForegroundWaitMs(bashCfg.foreground_wait_window_ms);
      // Only log when the gate actually changes behavior (subagent path).
      // The common primary-session foreground case is the overwhelming
      // majority of calls and produces no useful log signal.
      if (subagentForcedForeground && requestedBackground) {
        sessionLog(
          context.sessionID,
          "[bash] subagent + background:true → converting to foreground (subagent would lose task_id)",
        );
      }
      const shellEnv = await ctx.plugin?.trigger?.(
        "shell.env",
        { cwd, sessionID: context.sessionID, callID: getCallID(context) },
        { env: {} },
      );

      const removeAbortListener = listenForForegroundAbort(
        ctx,
        context,
        !effectiveBackground && !requestedPty,
      );
      let data: Awaited<ReturnType<typeof withPermissionLoop>>;
      let usedHostFallback = false;
      try {
        data = await withPermissionLoop(
          ctx,
          context,
          {
            command,
            timeout: rawTimeout,
            workdir: args.workdir,
            env: shellEnv?.env ?? {},
            description,
            background: effectiveBackground,
            notify_on_completion: effectiveBackground,
            compressed,
            pty: requestedPty,
            pty_rows: ptyRows,
            pty_cols: ptyCols,
            permissions_requested: true,
            foreground_orchestrate: true,
            block_to_completion: blockToCompletion,
            wait: requestedWait,
            sandbox: args.sandbox,
            ...(runon !== undefined ? { runon } : {}),
            ...(returnPaths !== undefined ? { return_paths: returnPaths } : {}),
          },
          callBashBridge,
          {
            transportTimeoutMs: orchestratedTransportTimeoutMs(
              blockToCompletion,
              requestedWait,
              rawTimeout,
              foregroundWaitMs,
              workerWaitMaxMs,
              runon !== undefined,
            ),
            onProgress: ({ text }) => {
              accumulatedOutput = preview(accumulatedOutput + text);
              void metadata({ output: accumulatedOutput, description });
            },
          },
        );
      } catch (error) {
        const fallbackCause = classifyBashHostFallbackError(error);
        if (fallbackCause === undefined) throw error;
        if (command.includes("{{ck-secret:")) {
          throw new Error(BASH_SECRET_HOST_FALLBACK_REFUSAL);
        }
        if (!backgroundDisabled && rawRequestedBackground) {
          throw refuseAftModuleOptionWithoutFallback(
            error,
            "background:true",
            BASH_HOST_FALLBACK_REFUSAL,
          );
        }
        if (requestedPty) {
          throw refuseAftModuleOptionWithoutFallback(error, "pty:true", BASH_HOST_FALLBACK_REFUSAL);
        }
        if (runon !== undefined) {
          throw refuseAftModuleOptionWithoutFallback(error, "runon", BASH_HOST_FALLBACK_REFUSAL);
        }
        if (returnPaths !== undefined) {
          throw refuseAftModuleOptionWithoutFallback(
            error,
            "return_paths",
            BASH_HOST_FALLBACK_REFUSAL,
          );
        }
        if (!bashCfg.host_fallback) throw error;
        if (bashCfg.memory_limit_mib !== undefined) {
          throw refuseAftModuleOptionWithoutFallback(
            error,
            "bash.memory_limit_mib",
            BASH_HOST_FALLBACK_REFUSAL,
          );
        }

        const projectRoot = projectRootFor(context);
        const pattern = bashHostFallbackAskPattern(command, projectRoot, fallbackCause);
        await runAsk(
          context.ask({
            permission: "bash",
            patterns: [pattern],
            always: [],
            metadata: { command, cwd: projectRoot, host_fallback: true },
          }),
        );
        data = await runBashHostFallback({
          command,
          projectRoot,
          timeoutMs: rawTimeout,
          signal: context.abort,
          env: shellEnv?.env,
        });
        usedHostFallback = true;
        hostFallbackActive = true;
      } finally {
        removeAbortListener();
      }

      if (data.success === false) {
        throw new Error((data.message as string) || "bash failed");
      }
      // The normal dispatch above is the foreground recovery probe. A successful
      // response means this command used the module, so do not retain its fallback banner.
      if (!usedHostFallback && hostFallbackActive) hostFallbackActive = false;

      const uiTitle = description ?? shortenCommand(command);
      if (isLiveBackgroundTaskReply(data)) {
        const taskId = data.task_id;
        trackBgTask(context.sessionID, taskId);
        await trackV2BackgroundTask(ctx, context, taskId, startedEpoch);
        let rendered = (data.output as string | undefined) ?? "";
        // Also when subagent background is off: a blocking call that reached
        // the worker wait limit hands back a still-running task the worker
        // must know how to wait on.
        if (isSubagent) {
          rendered += workerBackgroundTaskNote(taskId);
        }
        const metadataPayload = { description, output: rendered, status: "running", taskId };
        await metadata(metadataPayload);
        return { output: rendered, title: uiTitle, metadata: metadataPayload };
      }

      const output = (data.output as string | undefined) ?? "";
      const rendered = usedHostFallback
        ? output
        : maybeAppendGrepSearchHint(output, command, aftSearchRegistered, projectRootFor(context));
      const metadataPayload = foregroundMetadata(description, data, rendered);
      await metadata(metadataPayload);
      return {
        output: rendered,
        title: uiTitle,
        metadata: metadataPayload,
      };
    },
  };
}

export function createBashStatusTool(ctx: PluginContext): ToolDefinition {
  // Point at bash_watch for waiting only when the model can call it.
  const waitSteer = bashCompanionRegistered(ctx.config, "bash_watch")
    ? " To wait, use bash_watch."
    : "";
  return {
    description: `Read-only snapshot of a background or PTY bash task's current state and output. Returns immediately. Never waits. One look to check on a task is fine — never loop it to wait for completion.${waitSteer}`,
    args: {
      taskId: z
        .string()
        .describe(
          "Background task ID returned by bash({ background: true }), e.g. bash-6b454047a1c39ded.",
        ),
      outputOffset: optionalInt(0, Number.MAX_SAFE_INTEGER).describe(
        "Read a bounded stdout/PTY byte page from this offset; returns masked text and the next byte offset.",
      ),
      stderrOffset: optionalInt(0, Number.MAX_SAFE_INTEGER).describe(
        "Read a bounded stderr byte page from this offset; returns masked text and the next byte offset.",
      ),
      outputMode: z
        .enum(["screen", "raw", "both"])
        .optional()
        .describe(
          "PTY output rendering mode. Defaults to screen for PTY tasks and preserves existing behavior for piped tasks when omitted.",
        ),
    },
    execute: async (args, context) => {
      const taskId = args.taskId as string;
      const outputMode = args.outputMode as string | undefined;
      // bash_status is snapshot-only as of bash_watch landing. waitFor/exit/
      // timeoutMs moved to bash_watch — if the agent passes them here, they're
      // silently ignored at the Zod schema layer (extra keys stripped).
      const paging = args.outputOffset !== undefined || args.stderrOffset !== undefined;
      const data = paging
        ? await callBashBridge(ctx, context, "bash_status", {
            task_id: taskId,
            output_mode: outputMode,
            output_offset: coerceOptionalInt(
              args.outputOffset,
              "outputOffset",
              0,
              Number.MAX_SAFE_INTEGER,
            ),
            stderr_offset: coerceOptionalInt(
              args.stderrOffset,
              "stderrOffset",
              0,
              Number.MAX_SAFE_INTEGER,
            ),
          })
        : await bashStatusSnapshot(ctx, context, taskId, outputMode);
      if (data.success === false) throw new Error(String(data.message ?? "bash_status failed"));
      if (paging) return formatBashOutputPages(data, true);
      const isSubagent = await resolveIsSubagent(ctx.client, context.sessionID, context.directory);
      return await formatBashStatusText(
        context,
        taskId,
        data,
        outputMode,
        isSubagent ? "worker" : "primary",
      );
    },
  };
}

export function createBashKillTool(ctx: PluginContext): ToolDefinition {
  return {
    description:
      "Terminate a running background bash task spawned with bash({ background: true }). Returns confirmation of kill or an error if the task already finished.",
    args: {
      taskId: z
        .string()
        .describe(
          "Background task ID returned by bash({ background: true }), e.g. bash-6b454047a1c39ded.",
        ),
    },
    execute: async (args, context) => {
      const data = await callBashBridge(ctx, context, "bash_kill", {
        task_id: args.taskId as string,
      });
      if (data.success === false) {
        throw new Error((data.message as string | undefined) ?? "bash_kill failed");
      }
      const warning =
        typeof data.bot_write_revocation_warning === "string"
          ? `\n${data.bot_write_revocation_warning}`
          : "";
      if (data.kill_signaled === true) {
        return `Task ${args.taskId}: kill_signaled · reached ${String(data.kill_reached ?? 0)} live descendants${warning}`;
      }
      if (typeof data.remote_cancel === "string") {
        return `Task ${args.taskId}: ${String(data.status ?? "fate_unknown")} · ${data.remote_cancel}${warning}`;
      }
      return `Task ${args.taskId}: ${String(data.status ?? "killed")}${warning}`;
    },
  };
}

async function bashStatusSnapshot(
  ctx: PluginContext,
  runtime: ToolContext,
  taskId: string,
  outputMode: string | undefined,
  options?: BridgeRequestOptions,
): Promise<Record<string, unknown>> {
  const data = await callBashBridge(
    ctx,
    runtime,
    "bash_status",
    { task_id: taskId, output_mode: outputMode },
    options,
  );
  if (data.success === false) {
    throw new Error((data.message as string | undefined) ?? "bash_status failed");
  }
  return data;
}

async function formatBashStatusText(
  runtime: ToolContext,
  taskId: string,
  data: Record<string, unknown>,
  requestedOutputMode: string | undefined,
  role: WatchCallerRole,
): Promise<string> {
  const status = data.status as string;
  const exit = typeof data.exit_code === "number" ? ` (exit ${data.exit_code})` : "";
  const dur =
    typeof data.duration_ms === "number" ? ` ${Math.round(data.duration_ms / 1000)}s` : "";
  let text = `Task ${taskId}: ${status}${exit}${dur}`;
  if (typeof data.live_descendants_summary === "string") {
    text += ` · ${data.live_descendants_summary}`;
  }
  if (status === "running" && typeof data.remote_job_id === "string") {
    text += ` · remote job ${data.remote_job_id}`;
  }
  // A remote task's build-server status note: that it is still queued (and
  // for how long), or when the runner will stop it. Without this note a
  // queued job would show as plain "running".
  if (typeof data.remote_phase_note === "string") {
    text += ` · ${data.remote_phase_note}`;
  }
  if (data.output_incomplete === true || (status === "failed" && data.status_reason)) {
    const reason =
      typeof data.status_reason === "string" && data.status_reason
        ? data.status_reason
        : "PTY output may be incomplete";
    text += `\n[${reason}]`;
  }
  if (data.mode === "pty") {
    // PTY output is rendered from the raw terminal spill file; never feed it
    // through the piped-output compression/line renderer.
    text += await formatPtyStatus(runtime, taskId, data, requestedOutputMode);
  } else {
    const preview = data.output_preview as string | undefined;
    if (preview && status !== "running") {
      text += `\n${preview}`;
    }
    if (status === "running") {
      text += `\n${runningTaskStatusHint(role)}`;
    }
  }
  return text;
}

async function formatPtyStatus(
  _runtime: ToolContext,
  taskId: string,
  data: Record<string, unknown>,
  requestedOutputMode: string | undefined,
): Promise<string> {
  const outputMode = requestedOutputMode ?? "screen";
  const raw = typeof data.pty_raw === "string" ? data.pty_raw : "";
  let suffix = "";
  if (outputMode === "raw") {
    suffix = raw.length > 0 ? `\n${raw}` : "";
  } else if (outputMode === "both") {
    suffix = `\n${JSON.stringify({ screen: String(data.pty_screen ?? ""), raw }, null, 2)}`;
  } else {
    const screen = data.pty_screen as string | undefined;
    suffix = screen ? `\n${screen}` : "";
  }
  if (data.status === "running") {
    suffix += `\nPTY task is still running. Use bash_status({ taskId: "${taskId}", outputMode: "screen" }) to inspect, bash_write({ taskId: "${taskId}", input: "..." }) to send keystrokes.`;
  }
  return suffix;
}

function preview(output: string): string {
  return output.length <= METADATA_PREVIEW_LIMIT ? output : output.slice(-METADATA_PREVIEW_LIMIT);
}

function foregroundMetadata(
  description: string | undefined,
  data: Record<string, unknown>,
  rendered: string,
): Record<string, unknown> {
  const outputPath = data.output_path as string | undefined;
  const truncated =
    typeof data.truncated === "boolean"
      ? data.truncated
      : (data.output_truncated as boolean | undefined);
  return {
    description,
    output: preview(rendered),
    exit: data.exit_code as number | undefined,
    truncated,
    ...(outputPath ? { outputPath } : {}),
    ...(data._bash_changed_files ? { _bash_changed_files: data._bash_changed_files } : {}),
  };
}

function getCallID(ctx: unknown): string | undefined {
  const c = ctx as { callID?: string; callId?: string; call_id?: string };
  return c.callID ?? c.callId ?? c.call_id;
}

function shortenCommand(command: string): string {
  const collapsed = command.replace(/\s+/g, " ").trim();
  return collapsed.length <= 80 ? collapsed : `${collapsed.slice(0, 77)}...`;
}
