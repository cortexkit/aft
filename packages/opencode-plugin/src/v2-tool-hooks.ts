import type { SessionContext, SessionDomain } from "@opencode/plugin/effect/session";
import type { ToolDomain } from "@opencode/plugin/effect/tool";
import { Effect, Schema } from "effect";
import { appendInTurnBgCompletions, hasHeldBgCompletions } from "./bg-notifications.js";
import { sessionWarn } from "./logger.js";
import { prepareOpenCodeArguments } from "./normalize-schemas.js";
import { maybeAppendConflictsHint } from "./shared/bash-hints.js";
import { rememberReadModel } from "./shared/read-vision.js";
import { resolveSubagentStatus } from "./shared/subagent-detect.js";
import type { PluginContext } from "./types.js";
import {
  aftToolNameForV2Host,
  renameV2ToolReferences,
  v2HostToolName,
} from "./v2-host-tool-names.js";

// The host catches this tagged failure as a failed tool call, not a session
// defect. Use its public error shape without coupling to a second schema copy.
class ArgumentError extends Schema.TaggedError<ArgumentError>()("Tool.Error", {
  message: Schema.String,
}) {}

/**
 * Match the host's GPT patch preference without touching tools AFT did not
 * register. `registeredTools` holds AFT names; `event.tools` is keyed by the
 * names the host registered them under (`apply_patch` is `patch` there).
 */
function gateEditingTools(event: SessionContext, registeredTools: ReadonlySet<string>): void {
  const id = event.model.id;
  const prefersPatch = id.includes("gpt-") && !id.includes("oss") && !id.includes("gpt-4");
  for (const name of prefersPatch ? ["edit", "write"] : ["apply_patch"]) {
    if (registeredTools.has(name)) delete event.tools[v2HostToolName(name)];
  }
}

/**
 * How the host's own shell tool describes itself. While OpenCode 2's built-in
 * shell plugin is loaded (the user's config does not remove it with
 * `-opencode.tool.shell`), its request hooks rewrite the description of
 * whatever tool is named `shell` to this text on every model request, AFT's
 * included. Only a description that starts this way is treated as the host's
 * rewrite; another plugin's own `shell` is left alone.
 */
const HOST_SHELL_DESCRIPTION = /^Execute a shell command\b/;

/**
 * Put AFT's own description back on its `shell` tool after the host's
 * built-in shell plugin rewrote it. AFT's hooks are registered after the
 * built-in plugins' (the host loads built-ins first and runs hooks in
 * registration order), so this runs after the rewrite it undoes.
 */
function restoreShellDescription(
  event: SessionContext,
  registeredTools: ReadonlySet<string>,
  shellDescription: string | undefined,
): void {
  if (shellDescription === undefined || !registeredTools.has("bash")) return;
  const shell = event.tools[v2HostToolName("bash")];
  if (!shell || shell.description === shellDescription) return;
  if (!HOST_SHELL_DESCRIPTION.test(shell.description)) return;
  shell.description = shellDescription;
}

/**
 * Register the OpenCode 2 hooks that do what AFT's OpenCode 1 hooks do:
 * normalise a call's raw arguments before the host validates them, append the
 * merge-conflict hint to a command's result text, gate the editing tools by
 * model and remember the session's current model.
 *
 * `registeredTools` holds AFT tool names; the host's events name tools by the
 * name they are registered under, which differs for `bash` (`shell`) and
 * `apply_patch` (`patch`). `bashDescription` is the description AFT's bash
 * tool was built with, as written for OpenCode 1.
 */
export function registerV2ToolHooks(
  context: unknown,
  runtime: {
    hashlineEffective?: boolean;
    pool?: PluginContext["pool"];
    drainBgNotifications?: (sessionID: string) => Promise<void>;
  },
  registeredTools: ReadonlySet<string>,
  bashDescription?: string,
) {
  const host = context as { tool?: ToolDomain; session?: SessionDomain };
  const shellDescription =
    bashDescription === undefined ? undefined : renameV2ToolReferences(bashDescription);
  return Effect.gen(function* () {
    if (typeof host.tool?.hook === "function") {
      yield* host.tool.hook("execute.after", (event) =>
        Effect.promise(async () => {
          if (
            runtime.drainBgNotifications &&
            event.status === "completed" &&
            !hasHeldBgCompletions(event.sessionID)
          ) {
            // A subagent's drain can wait for the current execution to settle
            // after a tool abort. Joining it here would prevent that execution
            // from ending. Let the existing handler continue in the background
            // to deliver and acknowledge the completion after execution ends.
            // Only a proven primary may wait for it from a tool hook.
            if ((await resolveSubagentStatus(context, event.sessionID)) === false) {
              await runtime.drainBgNotifications(event.sessionID);
            } else {
              void runtime.drainBgNotifications(event.sessionID).catch((error) => {
                sessionWarn(
                  event.sessionID,
                  `Background notification drain failed: ${String(error)}`,
                );
              });
            }
            if (!hasHeldBgCompletions(event.sessionID)) return;
          }
          if (
            !runtime.pool ||
            event.status !== "completed" ||
            !hasHeldBgCompletions(event.sessionID)
          )
            return;
          const content = event.result.content;
          const output = { output: "" };
          await appendInTurnBgCompletions(
            {
              ctx: runtime as PluginContext,
              directory: (context as { location: { directory: string } }).location.directory,
              sessionID: event.sessionID,
            },
            output,
          );
          if (!output.output) return;
          event.result = {
            ...event.result,
            content:
              typeof content === "string"
                ? content + output.output
                : [...(content ?? []), { type: "text", text: output.output }],
          };
        }),
      );
      yield* host.tool.hook("execute.before", (event) =>
        Effect.try({
          try: () => {
            const name = aftToolNameForV2Host(event.tool);
            if (!registeredTools.has(name)) return;
            event.input = prepareOpenCodeArguments(name, event.input, {
              hashlineEffective: runtime.hashlineEffective,
            });
          },
          // Reject before the host can strip unknown fields from the raw input.
          catch: (error) =>
            new ArgumentError({ message: error instanceof Error ? error.message : String(error) }),
        }),
      );
      yield* host.tool.hook("execute.after", (event) =>
        Effect.sync(() => {
          if (
            aftToolNameForV2Host(event.tool) !== "bash" ||
            event.status !== "completed" ||
            !registeredTools.has("bash") ||
            !registeredTools.has("aft_conflicts")
          )
            return;
          const content = event.result.content;
          event.result = {
            ...event.result,
            content:
              typeof content === "string"
                ? maybeAppendConflictsHint(content)
                : content?.map((part) =>
                    part.type === "text"
                      ? { ...part, text: maybeAppendConflictsHint(part.text) }
                      : part,
                  ),
          };
        }),
      );
    }
    if (typeof host.session?.hook === "function") {
      // Each request starts with its own tool draft, including auxiliary requests.
      // Disabling the built-in patch plugin removes its gate, so AFT owns this one.
      for (const name of ["compaction", "generate"] as const) {
        yield* host.session.hook(name, (event) =>
          Effect.sync(() => {
            gateEditingTools(event, registeredTools);
            restoreShellDescription(event, registeredTools, shellDescription);
          }),
        );
      }
      yield* host.session.hook("context", (event) =>
        Effect.sync(() => {
          gateEditingTools(event, registeredTools);
          restoreShellDescription(event, registeredTools, shellDescription);
          rememberReadModel(context as object, event.sessionID, {
            providerID: event.model.providerID,
            modelID: event.model.id,
          });
        }),
      );
    }
  });
}
