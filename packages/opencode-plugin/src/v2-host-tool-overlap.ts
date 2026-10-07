/**
 * On OpenCode 2, AFT's `apply_patch` and `bash` sit beside the host's own
 * `patch` and `shell` tools unless the user's opencode config turns the host
 * plugins off (`-opencode.tool.patch` / `-opencode.tool.shell` under
 * `plugins`, which `aft setup` and `aft doctor --fix` write). Someone who
 * installed AFT without setup, or turned the host tools back on, gets two
 * editing tools and two shells with nothing saying why. This module tells the
 * user once per host process, in the chat, and points at `doctor --fix`.
 *
 * What counts is what the host actually registered: `context.tool.list()`
 * returns the host's tools after every plugin's transform, keyed by the name
 * the model sees. The check waits for the first user prompt rather than
 * running at plugin start, because at start other plugins (the host's own
 * patch and shell plugins included) may not have registered yet, and because
 * a chat notice needs a session to land in. A host without `tool.list()`
 * (OpenCode 2.0.3 has none) or a list call that fails gets no notice: the
 * reason is logged once at debug level and nothing is guessed.
 *
 * The notice reaches the user only: it is a non-resuming synthetic chat
 * record, the same path AFT's other OpenCode 2 notices use. No system prompt,
 * tool description or other prompt-cache input changes.
 */
import { Effect } from "effect";

import { debug, warn } from "./logger.js";
import { sendIgnoredMessage } from "./shared/ignored-message.js";
import { isAftOriginatedPrompt } from "./wakes/session-delivery.js";

/** An AFT tool and the OpenCode 2 built-in that does the same job. */
export interface HostToolOverlap {
  readonly aft: "apply_patch" | "bash";
  readonly host: "patch" | "shell";
}

const OVERLAPS: readonly HostToolOverlap[] = [
  { aft: "apply_patch", host: "patch" },
  { aft: "bash", host: "shell" },
];

const FIX_COMMAND = "`npx @cortexkit/aft doctor --fix`";

/**
 * The pairs where AFT registered its tool (it is not in `disabled_tools`) and
 * the host also has its built-in, in a fixed order.
 */
export function findHostToolOverlaps(
  hostToolNames: ReadonlySet<string>,
  registeredAftTools: ReadonlySet<string>,
): HostToolOverlap[] {
  return OVERLAPS.filter(
    (pair) => registeredAftTools.has(pair.aft) && hostToolNames.has(pair.host),
  );
}

/** One notice for every overlapping pair, or null when there is none. */
export function hostToolOverlapNotice(overlaps: readonly HostToolOverlap[]): string | null {
  const patch = overlaps.some((pair) => pair.host === "patch");
  const shell = overlaps.some((pair) => pair.host === "shell");
  if (patch && shell) {
    return `🔧 AFT: OpenCode's built-in patch and shell tools are still enabled beside AFT's apply_patch and bash, so the model sees two editing tools and two shells. Run ${FIX_COMMAND} to turn the built-in ones off.`;
  }
  if (patch) {
    return `🔧 AFT: OpenCode's built-in patch tool is still enabled beside AFT's apply_patch, so the model sees two editing tools. Run ${FIX_COMMAND} to turn the built-in one off.`;
  }
  if (shell) {
    return `🔧 AFT: OpenCode's built-in shell tool is still enabled beside AFT's bash, so the model sees two shells. Run ${FIX_COMMAND} to turn the built-in one off.`;
  }
  return null;
}

// Process-wide state: the host runs one plugin runtime per Location in the
// same process, and the user should see the notice once, not once per
// Location or session.
let noticeClaimed = false;
let unavailableLogged = false;

/** Reset the once-per-process state. Tests only. */
export function __resetHostToolOverlapNoticeForTests(): void {
  noticeClaimed = false;
  unavailableLogged = false;
}

function logDetectionUnavailable(reason: string): void {
  if (unavailableLogged) return;
  unavailableLogged = true;
  debug(`Built-in tool overlap check skipped: ${reason}; no notice shown`);
}

function reasonOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

interface ToolListEntry {
  readonly id?: unknown;
  readonly name?: unknown;
}

interface OverlapHostContext {
  readonly tool?: { readonly list?: unknown };
  readonly session?: { readonly hook?: unknown };
}

/**
 * The host's registered tool names, or null when the host cannot say. The
 * list is keyed by the name the model sees (`id`); `name` is only read when
 * an entry has no `id`.
 */
async function hostToolNames(context: OverlapHostContext): Promise<ReadonlySet<string> | null> {
  const tool = context.tool;
  const list = tool?.list;
  if (typeof list !== "function") {
    logDetectionUnavailable("the host context has no tool.list()");
    return null;
  }
  let entries: unknown;
  try {
    entries = await Effect.runPromise(
      (list as () => Effect.Effect<readonly ToolListEntry[]>).call(tool),
    );
  } catch (error) {
    logDetectionUnavailable(`tool.list() failed: ${reasonOf(error)}`);
    return null;
  }
  if (!Array.isArray(entries)) {
    logDetectionUnavailable("tool.list() did not return a list");
    return null;
  }
  const names = new Set<string>();
  for (const entry of entries as ToolListEntry[]) {
    const name = typeof entry?.id === "string" ? entry.id : entry?.name;
    if (typeof name === "string") names.add(name);
  }
  return names;
}

interface PromptEvent {
  readonly sessionID: string;
  readonly metadata?: Record<string, unknown>;
}

/**
 * Check one Location's host once and, if a built-in still runs beside AFT's
 * replacement and no notice has gone out in this process, send it to the
 * session whose prompt triggered the check.
 */
async function checkAndNotify(
  context: OverlapHostContext,
  registeredAftTools: ReadonlySet<string>,
  sessionID: string,
): Promise<void> {
  const names = await hostToolNames(context);
  if (!names) return;
  const overlaps = findHostToolOverlaps(names, registeredAftTools);
  const text = hostToolOverlapNotice(overlaps);
  if (!text) return;
  if (noticeClaimed) return;
  noticeClaimed = true;
  try {
    await sendIgnoredMessage(context, sessionID, text);
    debug(
      `Built-in tool overlap notice sent to session ${sessionID}: ${overlaps
        .map((pair) => `${pair.host} beside ${pair.aft}`)
        .join(", ")}`,
    );
  } catch (error) {
    // Let a later prompt try again rather than lose the notice for good.
    noticeClaimed = false;
    warn(`Built-in tool overlap notice could not be delivered: ${reasonOf(error)}`);
  }
}

/**
 * The session `prompt` hook for one Location. It never fails and never waits:
 * the host holds the prompt until the hook returns, so the check runs in the
 * background after it.
 */
export function createHostToolOverlapPromptHook(
  context: unknown,
  registeredAftTools: ReadonlySet<string>,
  onChecked?: (done: Promise<void>) => void,
): (event: PromptEvent) => Effect.Effect<void> {
  const host = context as OverlapHostContext;
  let checked = false;
  return (event) =>
    Effect.sync(() => {
      if (checked || noticeClaimed) return;
      // A completion wake AFT admitted itself is not the user at the keyboard.
      if (isAftOriginatedPrompt(event?.metadata)) return;
      if (typeof event?.sessionID !== "string" || event.sessionID.length === 0) return;
      checked = true;
      const done = checkAndNotify(host, registeredAftTools, event.sessionID).catch((error) => {
        warn(`Built-in tool overlap check failed: ${reasonOf(error)}`);
      });
      onChecked?.(done);
    });
}

type PromptHookRegistrar = (
  name: "prompt",
  callback: (event: PromptEvent) => Effect.Effect<void>,
) => Effect.Effect<unknown, never, unknown>;

/**
 * Register the overlap check on an OpenCode 2 host's session `prompt` hook.
 * `registeredAftTools` is AFT's final tool surface, so a tool listed in
 * `disabled_tools` never counts as an overlap.
 */
export function registerV2HostToolOverlapNotice(
  context: unknown,
  registeredAftTools: ReadonlySet<string>,
  onChecked?: (done: Promise<void>) => void,
): Effect.Effect<boolean, never, unknown> {
  if (findHostToolOverlaps(new Set(["patch", "shell"]), registeredAftTools).length === 0) {
    // Neither apply_patch nor bash is registered: nothing can overlap.
    return Effect.succeed(false);
  }
  const session = (context as OverlapHostContext | null)?.session;
  const hook = session?.hook;
  if (typeof hook !== "function") {
    logDetectionUnavailable("the host context has no session prompt hook to deliver a notice from");
    return Effect.succeed(false);
  }
  return (hook as PromptHookRegistrar)
    .call(
      session,
      "prompt",
      createHostToolOverlapPromptHook(context, registeredAftTools, onChecked),
    )
    .pipe(Effect.as(true));
}
