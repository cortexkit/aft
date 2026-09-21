import { fail } from "./errors.js";
import type { HostEvent } from "./event-stream.js";
import type { ApiControlPlan, ScenarioDefinition, SessionPermissionRule } from "./types.js";
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

/** The id of the call the scenario's permission rules are there to gate. */
export function gatedCallId(scenario: ScenarioDefinition): string | undefined {
  for (const turn of scenario.turns) {
    if (turn.response.kind !== "tool_calls") continue;
    for (const call of turn.response.calls) {
      if (scenarioToolName(call.name) === scenario.tool) return call.id;
    }
  }
  return undefined;
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

/**
 * The controls a scenario runs against the host.
 *
 * A permission scenario adds none: its prompt is answered by the run client
 * itself, so there is no pending request for an external control to reply to.
 */
export function controlPlans(scenario: ScenarioDefinition): ApiControlPlan[] {
  return [...(scenario.controls ?? [])];
}
