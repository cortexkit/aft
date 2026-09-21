import { fail } from "./errors.js";
import type { HostEvent } from "./event-stream.js";
import { hostToolName } from "./mock-server.js";
import type {
  ApiControlPlan,
  RecordedMockExchange,
  ScenarioDefinition,
  SessionPermissionRule,
  ToolCallPlan,
} from "./types.js";
import { asRecord } from "./util.js";

/**
 * The permission action the AFT plugin asks for when a scenario's operation
 * runs.
 *
 * Every mutating tool routes through one `edit` ask, so a rule naming the tool
 * would never match anything; `read` and `bash` ask under their own names. The
 * input is the inventory operation, which is the scenario's own label for the
 * ask site under test.
 */
export function permissionAction(operation: string): string {
  if (operation.startsWith("bash:")) return "bash";
  if (operation === "read") return "read";
  return "edit";
}

/**
 * The ruleset a permission scenario needs on its session, or nothing.
 *
 * The session is the only ruleset channel the plugin reads: it appends the
 * session's own rules after the agent's and takes the last match, so a rule
 * installed here outranks the host's default allow-everything entry. The
 * host evaluates the same rules when the plugin asks it to raise a prompt,
 * which is what makes one ruleset enough for both sides.
 *
 * The resource is `*` because the tools do not agree on how they name one:
 * `edit` asks about a project-relative path, `aft_delete` and `aft_move` ask
 * about absolute paths, and `bash` asks about the command line. A rule naming
 * the scenario's declared path would match some of them and silently miss the
 * rest, which reads exactly like a tool that never asked.
 */
export function sessionPermissionRules(
  scenario: ScenarioDefinition,
): SessionPermissionRule[] | undefined {
  const permission = asRecord(scenario.metadata?.permission);
  if (!permission) return undefined;
  const operation = permission.operation;
  if (typeof operation !== "string") return undefined;
  const reply = permission.reply;
  const effect =
    reply === "once" || reply === "reject" ? "ask" : reply === "config_deny" ? "deny" : undefined;
  if (!effect) return undefined;
  return [{ action: permissionAction(operation), resource: "*", effect }];
}

function scenarioToolName(name: string): string {
  const bare = name.startsWith("aft_") ? name.slice(4) : name;
  if (bare === "ast_grep_search") return "ast_search";
  if (bare === "ast_grep_replace") return "ast_replace";
  return bare;
}

/** The call the scenario's permission rules are there to gate. */
function gatedCall(scenario: ScenarioDefinition): ToolCallPlan | undefined {
  for (const turn of scenario.turns) {
    if (turn.response.kind !== "tool_calls") continue;
    for (const call of turn.response.calls) {
      if (scenarioToolName(call.name) === scenario.tool) return call;
    }
  }
  return undefined;
}

/** The id of the call the scenario's permission rules are there to gate. */
export function gatedCallId(scenario: ScenarioDefinition): string | undefined {
  return gatedCall(scenario)?.id;
}

/**
 * The tool names the host offered the model in one recorded request.
 *
 * The host builds this list from its own registry each time it asks the model
 * for a response, so a recorded request is the registry as the host held it at
 * that moment. It is the only place the harness can read which tools a session
 * actually had, as opposed to which ones a call happened to reach.
 */
export function hostOfferedTools(request: unknown): string[] {
  const tools = asRecord(request)?.tools;
  if (!Array.isArray(tools)) return [];
  const names: string[] = [];
  for (const entry of tools) {
    const record = asRecord(entry);
    const name = asRecord(record?.function)?.name ?? record?.name;
    if (typeof name === "string") names.push(name);
  }
  return names;
}

/**
 * Assert that a wholly-denying rule HID the tool it names.
 *
 * The host does not answer a denied call by running the tool and refusing it.
 * It drops the tool from the session's snapshot entirely: every tool whose own
 * `options.permission` is denied outright stops being offered to the model
 * from the next request onward. So the rule's effect is visible in the host's
 * tool list, not only in the call's answer.
 *
 * Reading it from the list is also what keeps two different outcomes apart. A
 * tool the plugin never registered is likewise absent and likewise never runs,
 * which is how a scenario can look denied while actually testing nothing. The
 * rules are installed from the first scripted turn, after the host has already
 * built that turn's request, so the first request is the one that shows what
 * the host had BEFORE the denial: the tool must be there, and gone afterwards.
 */
export function assertConfigDenyHidesTool(
  scenario: ScenarioDefinition,
  exchanges: readonly RecordedMockExchange[],
): void {
  if (asRecord(scenario.metadata?.permission)?.reply !== "config_deny") return;
  const call = gatedCall(scenario);
  if (!call) {
    fail("scenario_invalid", `${scenario.id}: permission scenario has no subject tool call`);
  }
  const tool = hostToolName(call.name);
  const [first, ...later] = exchanges;
  if (!first) {
    fail("host_failed", `${scenario.id}: the host asked the model for nothing, so it offered no tools`);
  }
  const offeredBefore = hostOfferedTools(first.request);
  if (!offeredBefore.includes(tool)) {
    fail(
      "host_failed",
      `${scenario.id}: the host never offered ${tool}, so the deny rule hid nothing`,
      { turn: first.label, offered: offeredBefore },
    );
  }
  if (later.length === 0) {
    fail(
      "scenario_invalid",
      `${scenario.id}: a config_deny row needs a turn after the denied call, so the host rebuilds its tool list under the rule`,
    );
  }
  const stillOffering = later.filter((exchange) =>
    hostOfferedTools(exchange.request).includes(tool),
  );
  if (stillOffering.length > 0) {
    fail(
      "no_effect_observed",
      `${scenario.id}: the host kept offering ${tool} after the deny rule was installed`,
      { turns: stillOffering.map((exchange) => exchange.label) },
    );
  }
}

/**
 * Assert that the host raised and answered the prompt the scenario declares.
 *
 * The evidence is the host's own event stream, recorded from before the client
 * started: `permission.asked` says the plugin put the question to the host for
 * this tool call, and `permission.replied` says how it was answered. Both are
 * needed. An approved prompt is answered so fast that a poll of the pending
 * list never sees it, and a tool that completed proves nothing on its own,
 * because a tool that never asked also completes.
 *
 * The request is matched by its source call id rather than its resource, since
 * that id is the scenario's own tool call and is the same whatever shape the
 * tool gives its resources.
 *
 * A configured denial is decided before anything is raised, so it declares no
 * events and none are required.
 */
export function assertPermissionPromptObserved(
  scenario: ScenarioDefinition,
  events: readonly HostEvent[],
): void {
  const permission = asRecord(scenario.metadata?.permission);
  const rules = sessionPermissionRules(scenario);
  const reply = permission?.reply;
  if (!rules || (reply !== "once" && reply !== "reject")) return;
  const action = rules[0].action;
  const callId = gatedCallId(scenario);
  if (!callId) {
    fail("scenario_invalid", `${scenario.id}: permission scenario has no subject tool call`);
  }

  const asked = events.find(
    (event) =>
      event.type === "permission.asked" &&
      event.data.action === action &&
      asRecord(event.data.source)?.id === callId,
  );
  if (!asked) {
    fail(
      "no_effect_observed",
      `${scenario.id}: the host recorded no ${action} permission request from ${callId}`,
      {
        observed: events
          .filter((event) => event.type === "permission.asked")
          .map((event) => event.data),
      },
    );
  }

  const answered = events.find(
    (event) =>
      event.type === "permission.replied" &&
      event.data.requestID === asked.data.id &&
      event.data.reply === reply,
  );
  if (!answered) {
    fail(
      "no_effect_observed",
      `${scenario.id}: the permission request was not answered ${String(reply)}`,
      {
        request_id: asked.data.id,
        observed: events.filter((event) => event.type === "permission.replied"),
      },
    );
  }
}

/** What the plugin puts in the break-glass ask it raises when AFT is unreachable. */
export const HOST_FALLBACK_ASK_MARKER = "AFT UNAVAILABLE";

/** The banner the plugin puts on output it produced without AFT. */
export const HOST_FALLBACK_OUTPUT_MARKER = "[AFT host fallback";

export interface BashPermissionPathObservation {
  /** Everything the scripted calls returned to the model. */
  resultText: string;
  /** The host's own event stream for this scenario. */
  events: readonly HostEvent[];
}

/** True when the plugin's break-glass path asked for, or produced, anything. */
export function bashHostFallbackObserved(observed: BashPermissionPathObservation): boolean {
  if (observed.resultText.includes(HOST_FALLBACK_OUTPUT_MARKER)) return true;
  return observed.events.some(
    (event) =>
      event.type === "permission.asked" &&
      JSON.stringify(event.data).includes(HOST_FALLBACK_ASK_MARKER),
  );
}

/** Which of the two declared bash execution paths a row is, if either. */
function bashPermissionFamily(scenario: ScenarioDefinition): "loop" | "fallback" | undefined {
  if (scenario.id.startsWith("bash/T3/loop_")) return "loop";
  if (scenario.id.startsWith("bash/T3/fallback_")) return "fallback";
  return undefined;
}

/** True when the row's permission answer lets the command run. */
function bashPermissionAllowed(scenario: ScenarioDefinition): boolean {
  return asRecord(scenario.metadata?.permission)?.reply === "once";
}

/**
 * Assert which of the two execution paths a bash permission row took.
 *
 * The two families are selected by declared configuration, and each has its
 * own ask site. The loop family keeps AFT's transport alive, so the engine
 * declares the asks and AFT executes what the answer allows. The fallback
 * family kills that transport, so the only way the command can run is the
 * plugin's break-glass path, which announces itself in the ask it raises
 * ("AFT UNAVAILABLE") and banners the output it returns. A row wearing the
 * other family's mark ran the other path, whatever its file effects look like.
 *
 * A fallback row is asserted to REACH that ask whether its answer allows the
 * command or refuses it: the ask is the site the row exists to cover, and a
 * refusal only means the command was not run after it was raised. The one
 * fallback row that must not see it is the configured denial, which the host
 * settles before the tool is dispatched at all — an ask there would be a way
 * around the rule rather than the path under test.
 *
 * This half is asserted as soon as the host has finished, before the disk
 * proofs read their snapshots, because a row that took the wrong path makes
 * every later file claim unreadable.
 */
export function assertBashFallbackAskIdentity(
  scenario: ScenarioDefinition,
  observed: BashPermissionPathObservation,
): void {
  const family = bashPermissionFamily(scenario);
  if (!family) return;
  const configured = asRecord(scenario.metadata?.permission)?.reply === "config_deny";
  const details = { result_text: observed.resultText.slice(0, 600) };
  if (!bashHostFallbackObserved(observed)) {
    if (family === "fallback" && !configured) {
      fail(
        "no_effect_observed",
        `${scenario.id}: the dead transport raised no host-fallback ask, so bash refused the command instead of falling back`,
        details,
      );
    }
    return;
  }
  if (family === "loop") {
    fail("no_effect_observed", `${scenario.id}: the loop row fell back to host execution`, details);
  }
  if (configured) {
    fail(
      "no_effect_observed",
      `${scenario.id}: a rule-denied command raised the host-fallback ask, which would run it anyway`,
      details,
    );
  }
}

/**
 * Assert whether AFT executed the row's command, from the task rows it keeps.
 *
 * The other half of the path identity above, and the half that has to wait
 * until the run is over: the task rows are read from AFT's own database once
 * its processes have been confirmed stopped.
 *
 * The loop rows used to require the pair `permission_required` -> `retry` in
 * the plugin log instead. Nothing writes those words to that log:
 * `permission_required` is a response code the engine returns for asks it
 * declares itself, which the plugin answers in memory. The gate these rows
 * actually exercise is the host's own rule against AFT's bash tool, which the
 * host raises and the run client answers; the permission events assert that
 * pair, and a task row is what proves the answer let AFT run the command.
 */
export function assertBashAftExecutionIdentity(
  scenario: ScenarioDefinition,
  taskIds: readonly string[],
): void {
  const family = bashPermissionFamily(scenario);
  if (!family) return;
  const allowed = bashPermissionAllowed(scenario);
  const details = { task_ids: [...taskIds] };
  if (!allowed) {
    if (taskIds.length > 0) {
      fail(
        "no_effect_observed",
        `${scenario.id}: a denied command still reached AFT, which left a task row`,
        details,
      );
    }
    return;
  }
  if (family === "loop" && taskIds.length === 0) {
    fail(
      "no_effect_observed",
      `${scenario.id}: AFT ran no command for the allowed call, so it left no task row`,
      details,
    );
  }
  if (family === "fallback" && taskIds.length > 0) {
    fail(
      "no_effect_observed",
      `${scenario.id}: AFT executed the command, so the transport-dead window did not take`,
      details,
    );
  }
}

/**
 * The controls a scenario runs against the host.
 *
 * A permission scenario adds none: its prompt is answered by the run client
 * itself, so there is no pending request for an external control to reply to.
 */
export function controlPlans(scenario: ScenarioDefinition): ApiControlPlan[] {
  return [...(scenario.controls ?? [])];
}
