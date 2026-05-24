import type {
  AgentToolResult,
  ExtensionAPI,
  ExtensionContext,
  Theme,
} from "@earendil-works/pi-coding-agent";
import { Container, Spacer, Text } from "@earendil-works/pi-tui";
import { type Static, Type } from "typebox";
import { trackBgTask } from "../bg-notifications.js";
import type { PluginContext } from "../types.js";
import { bridgeFor, callBridge, resolveSessionId } from "./_shared.js";

// Foreground polling wait-window: how long the plugin blocks the agent before
// promoting the task to background and returning. INTENTIONALLY decoupled
// from the task's own kill cap (`params.timeout`). Council decision:
// .alfonso/athena/council-aft-bash-timeout-design-5f25c3ee503ab303/
const FOREGROUND_WAIT_WINDOW_MS = 5_000;
const FOREGROUND_POLL_INTERVAL_MS = 100;
// Bridge transport budget for `bash` calls. Rust returns `running` immediately
// and the plugin polls separately, so transport only needs to cover spawn +
// protocol round-trip; not a function of params.timeout. See council audit
// `.alfonso/athena/council-aft-bash-timeout-audit-057818e1583d3883/`.
const BASH_TRANSPORT_TIMEOUT_MS = 30_000;

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

const BashParams = Type.Object({
  command: Type.String({
    description: "Shell command to execute. Supports pipes, redirections, and shell syntax.",
  }),
  timeout: Type.Optional(
    Type.Integer({
      minimum: 1,
      description:
        "Hard kill cap in milliseconds (positive integer). When omitted, the task can run up to 30 minutes. Foreground bash returns inline if the command finishes within ~5s; otherwise it's automatically promoted to background and a completion reminder is delivered when the task actually finishes.",
    }),
  ),
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
  background: Type.Optional(
    Type.Boolean({
      description:
        "Spawn command in background and return immediately with a task_id. Use bash_status to poll completion and bash_kill to terminate. Ideal for long-running tasks like builds or dev servers.",
    }),
  ),
  compressed: Type.Optional(
    Type.Boolean({
      description:
        "Compress output by removing ANSI codes, carriage returns, and excessive blank lines. Default: true. Set to false for raw terminal output including color codes.",
    }),
  ),
});

const BashTaskParams = Type.Object({
  task_id: Type.String({
    description: "Background bash task id returned by bash({ background: true }).",
  }),
});

interface BashDetails {
  exit_code?: number;
  duration_ms?: number;
  truncated?: boolean;
  output_path?: string;
  task_id?: string;
  bg_completions?: BgCompletion[];
}

interface BashStatusDetails {
  success: boolean;
  status: string;
  exit_code?: number;
  duration_ms?: number;
  output_preview?: string;
  command?: string;
}

interface BashKillDetails {
  success: boolean;
  status: string;
}

/** Local shape for Pi's render context — mirrors hoisted.ts pattern. */
interface RenderContextLike {
  lastComponent: import("@earendil-works/pi-tui").Component | undefined;
  isError: boolean;
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

export function registerBashTool(pi: ExtensionAPI, ctx: PluginContext): void {
  const spawnHook = getBashSpawnHook(pi);

  pi.registerTool<typeof BashParams, BashDetails>({
    name: "bash",
    label: "bash",
    description:
      "Execute shell commands through AFT's Rust bash handler. By default, output is compressed. Pass `compressed: false` for raw output. Pass `background: true` to spawn in the background and get a task_id for `bash_status`/`bash_kill`.",
    promptSnippet:
      "Run shell commands (timeout in milliseconds; supports workdir, background tasks, compressed output)",
    promptGuidelines: [
      "Use bash only when a dedicated AFT tool is not a better fit.",
      "Set compressed: false when you need ANSI color codes in the output.",
    ],
    parameters: BashParams,
    async execute(_toolCallId, params: Static<typeof BashParams>, _signal, onUpdate, extCtx) {
      const bridge = bridgeFor(ctx, extCtx.cwd);

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

      let streamed = "";
      const response = await callBridge(
        bridge,
        "bash",
        {
          command: spawnContext.command,
          timeout: params.timeout,
          workdir: spawnContext.cwd ?? params.workdir,
          env: spawnContext.env,
          description: params.description,
          background: params.background,
          notify_on_completion: params.background === true,
          compressed: params.compressed,
        },
        extCtx,
        {
          transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS,
          // Rust bash has its own watchdog that kills the child shell on the
          // bash-level timeout and returns a normal timed_out response well
          // before our transport timeout fires. If we hit the transport
          // deadline anyway it means the response is just late — don't
          // sacrifice the bridge (and all its warm state) for that.
          keepBridgeOnTimeout: true,
          onProgress: ({ text }) => {
            streamed += text;
            // Stream truncated output to avoid overwhelming the UI
            const displayText = truncateToVisualLines(streamed, 100);
            onUpdate?.(bashResult(displayText, { streaming: true }));
          },
        },
      ).catch((err) => {
        if (err instanceof Error && err.message.includes("permission_required")) {
          // Pi has no permission system — this should never reach us from Rust
          // (Track C scan returns empty for Pi). If it somehow did, throw clearly.
          throw new Error(
            "Permission ask reached Pi adapter — this is a bug. Pi has no permission system.",
          );
        }
        throw err;
      });

      if (response.success === false) {
        throw new Error((response.message as string | undefined) ?? "bash failed");
      }

      const taskId = response.task_id as string | undefined;
      if (response.status === "running" && taskId) {
        if (params.background === true) {
          trackBgTask(resolveSessionId(extCtx), taskId);
          return bashResult(formatBackgroundLaunch(taskId), { task_id: taskId });
        }

        // Wait-window decoupled from params.timeout. Always cap polling at
        // FOREGROUND_WAIT_WINDOW_MS so agents get a fast promotion message
        // for unexpectedly long commands. Honor a shorter explicit timeout
        // when present — polling beyond the task's kill cap is pointless.
        // Schema validation guarantees params.timeout is a positive integer
        // or undefined, so this Math.min is always well-defined.
        const waitTimeoutMs =
          params.timeout !== undefined
            ? Math.min(params.timeout, FOREGROUND_WAIT_WINDOW_MS)
            : FOREGROUND_WAIT_WINDOW_MS;
        const startedAt = Date.now();
        while (true) {
          const status = await callBridge(bridge, "bash_status", { task_id: taskId }, extCtx);
          if (status.success === false) {
            throw new Error((status.message as string | undefined) ?? "bash_status failed");
          }
          if (isTerminalStatus(status.status)) {
            return bashResult(formatForegroundResult(status), {
              exit_code: status.exit_code as number | undefined,
              duration_ms: status.duration_ms as number | undefined,
              truncated: status.output_truncated as boolean | undefined,
              output_path: status.output_path as string | undefined,
              task_id: taskId,
            });
          }
          if (Date.now() - startedAt >= waitTimeoutMs) {
            const promoted = await callBridge(bridge, "bash_promote", { task_id: taskId }, extCtx);
            if (promoted.success === false) {
              throw new Error((promoted.message as string | undefined) ?? "bash_promote failed");
            }
            trackBgTask(resolveSessionId(extCtx), taskId);
            return bashResult(formatPromotionMessage(taskId, params.timeout), {
              task_id: taskId,
            });
          }
          await sleep(FOREGROUND_POLL_INTERVAL_MS);
        }
      }

      const details: BashDetails = {
        exit_code: response.exit_code as number | undefined,
        duration_ms: response.duration_ms as number | undefined,
        truncated: response.truncated as boolean | undefined,
        output_path: response.output_path as string | undefined,
        task_id: taskId,
      };

      const output = (response.output as string | undefined) ?? "";
      return bashResult(output, details);
    },
    renderCall(args, theme, context) {
      return renderBashCall(args?.command, args?.description, theme, context);
    },
    renderResult(result, _options, theme, context) {
      return renderBashResult(result, theme, context);
    },
  });

  // bash_status and bash_kill ride alongside `bash` regardless of which
  // experimental flag enabled it: foreground bash auto-promotes long-running
  // tasks to background after a short wait-window, so the agent always needs
  // a way to inspect or kill promoted tasks. The `experimental.bash.background`
  // flag only gates explicit `bash({ background: true })` spawning, not the
  // promotion path.
  pi.registerTool<typeof BashTaskParams, BashStatusDetails>(createBashStatusTool(ctx));
  pi.registerTool<typeof BashTaskParams, BashKillDetails>(createBashKillTool(ctx));
}

function formatBackgroundLaunch(taskId: string): string {
  return `Background task started: ${taskId}. A completion reminder will be delivered automatically; don't poll bash_status.`;
}

function formatPromotionMessage(taskId: string, timeout: number | undefined): string {
  // Reports actual elapsed wait, not the user's full kill cap. The agent
  // already has the original command in its tool-call args; bash_status
  // returns it on demand if a downstream tool ever needs it.
  const waited =
    timeout !== undefined
      ? Math.min(timeout, FOREGROUND_WAIT_WINDOW_MS)
      : FOREGROUND_WAIT_WINDOW_MS;
  return `Foreground bash didn't finish within ${waited}ms and was promoted to background: ${taskId}. A completion reminder will be delivered automatically; use bash_status({ task_id: "${taskId}" }) to inspect output or bash_kill({ task_id: "${taskId}" }) to terminate.`;
}

function formatForegroundResult(data: Record<string, unknown>): string {
  const output = (data.output_preview as string | undefined) ?? "";
  const outputPath = data.output_path as string | undefined;
  const truncated = data.output_truncated === true;
  const status = data.status as string | undefined;
  const exit = data.exit_code as number | undefined;
  let rendered = output;
  if (truncated && outputPath) {
    rendered += `\n[output truncated; full output at ${outputPath}]`;
  }
  if (status === "timed_out") {
    rendered += `\n[command timed out]`;
  }
  if (typeof exit === "number" && exit !== 0) {
    rendered += `\n[exit code: ${exit}]`;
  }
  return rendered;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export function createBashStatusTool(ctx: PluginContext) {
  return {
    name: "bash_status",
    label: "bash_status",
    description:
      "Check the status of a background bash task spawned with bash({ background: true }).",
    promptSnippet: "Poll a background bash task by task_id",
    parameters: BashTaskParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof BashTaskParams>,
      _signal: AbortSignal | undefined,
      _onUpdate: ((update: AgentToolResult<BashStatusDetails>) => void) | undefined,
      extCtx: ExtensionContext,
    ) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const data = await callBridge(bridge, "bash_status", { task_id: params.task_id }, extCtx);
      if (data.success === false) {
        throw new Error((data.message as string | undefined) ?? "bash_status failed");
      }
      const details = data as unknown as BashStatusDetails;
      return bashStatusResult(formatBashStatus(params.task_id, details), details);
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
      const data = await callBridge(bridge, "bash_kill", { task_id: params.task_id }, extCtx);
      if (data.success === false) {
        throw new Error((data.message as string | undefined) ?? "bash_kill failed");
      }
      const details = data as unknown as BashKillDetails;
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

function formatBashStatus(taskId: string, details: BashStatusDetails): string {
  const exit = typeof details.exit_code === "number" ? ` (exit ${details.exit_code})` : "";
  const dur =
    typeof details.duration_ms === "number" ? ` ${Math.round(details.duration_ms / 1000)}s` : "";
  let text = `Task ${taskId}: ${details.status}${exit}${dur}`;
  if (isTerminalStatus(details.status) && details.output_preview) {
    text += `\n${details.output_preview.slice(0, 2000)}`;
  }
  // Anti-polling reminder for still-running tasks. Mirrors OpenCode plugin
  // wording exactly so agent behavior is consistent across both harnesses.
  // Terminal statuses get no suffix so the agent can consume the result
  // cleanly when the task is actually done.
  if (!isTerminalStatus(details.status)) {
    text += `\nA completion reminder will be delivered automatically; don't poll.`;
  }
  return text;
}

function isTerminalStatus(status: unknown): boolean {
  // Explicit allowlist (parity with opencode-plugin) so an unexpected status
  // string from Rust doesn't accidentally end the foreground polling loop.
  return (
    status === "completed" || status === "failed" || status === "killed" || status === "timed_out"
  );
}

function renderBashCall(
  command: string | undefined,
  description: string | undefined,
  theme: Theme,
  context: RenderContextLike,
): Text {
  const text = reuseText(context.lastComponent);
  const display = description ?? (command ? shortenCommand(command) : "...");
  text.setText(`${theme.fg("toolTitle", theme.bold("bash"))} ${theme.fg("accent", display)}`);
  return text;
}

function renderBashResult(
  result: AgentToolResult<BashDetails>,
  theme: Theme,
  context: RenderContextLike,
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

  // Output preview — last 25 lines, matching Pi built-in bash behaviour
  const rawOutput = result.content
    .filter((c) => c.type === "text")
    .map((c) => (c as { text?: string }).text ?? "")
    .join("\n")
    .trim();
  if (rawOutput) {
    const lines = rawOutput.split("\n");
    const preview =
      lines.length > 25
        ? `... (${lines.length - 25} lines omitted)\n${lines.slice(-25).join("\n")}`
        : rawOutput;
    container.addChild(new Text(preview, 1, 0));
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

  return container;
}

function shortenCommand(command: string): string {
  // Truncate long commands for UI display
  if (command.length <= 60) return command;
  return `${command.slice(0, 57)}...`;
}
