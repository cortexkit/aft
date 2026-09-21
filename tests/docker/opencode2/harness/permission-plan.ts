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
 * events and none are required. So is a row whose declared outcome is a
 * refusal: the command is declined before either ask site, so the session's
 * rule is installed and never consulted. That absence is not taken on trust
 * here - `assertBashDeadTransportRefusal` asserts that nothing was asked for
 * that call at all, which is a stronger claim than this one.
 */
export function assertPermissionPromptObserved(
  scenario: ScenarioDefinition,
  events: readonly HostEvent[],
): void {
  const permission = asRecord(scenario.metadata?.permission);
  const rules = sessionPermissionRules(scenario);
  const reply = permission?.reply;
  if (!rules || (reply !== "once" && reply !== "reject")) return;
  if (
    bashPermissionFamily(scenario) === "fallback" &&
    bashDeadTransportDeclaration(scenario).outcome === "refusal"
  ) {
    return;
  }
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

/**
 * What a fallback row declares its dead transport produces.
 *
 * `host_fallback` is the break-glass path executing the command in the host.
 * `refusal` is bash declining to run it at all, which is the correct outcome
 * for a transport failure whose result cannot be determined: the command may
 * already have been sent, and running it again would apply a mutation twice.
 * `not_dispatched` is the configured denial, settled by the host before the
 * tool runs, so no transport state is reached.
 */
export type BashDeadTransportOutcome = "host_fallback" | "refusal" | "not_dispatched";

export interface BashDeadTransportDeclaration {
  outcome: BashDeadTransportOutcome;
  /** Patterns the refusal handed back to the model must all match. */
  refusalNames: string[];
  /** `n/a:<reason>` when the row does not exercise break-glass execution. */
  hostFallbackCoverage?: string;
}

/**
 * Read a fallback row's declaration of what its dead transport produces.
 *
 * Declared per row rather than derived from the id, because the two are not
 * the same claim: the id says which path the row configures, and this says
 * what that path is expected to do on the transport the harness can actually
 * produce. A row that expects a refusal also has to record why break-glass
 * execution is not covered instead, so the capability is visibly accounted
 * for rather than quietly dropped.
 */
export function bashDeadTransportDeclaration(
  scenario: ScenarioDefinition,
): BashDeadTransportDeclaration {
  const declared = asRecord(scenario.metadata?.dead_transport);
  const outcome = declared?.outcome;
  if (
    !declared ||
    (outcome !== "host_fallback" && outcome !== "refusal" && outcome !== "not_dispatched")
  ) {
    fail(
      "scenario_invalid",
      `${scenario.id}: a fallback row must declare what its dead transport produces`,
      { declared },
    );
  }
  const refusalNames = Array.isArray(declared.refusal_names)
    ? declared.refusal_names.filter((name): name is string => typeof name === "string")
    : [];
  if (outcome === "refusal" && refusalNames.length === 0) {
    fail(
      "scenario_invalid",
      `${scenario.id}: a refusal row must declare what the refusal has to name`,
    );
  }
  const coverage = declared.host_fallback_coverage;
  const reason = declared.reason;
  if (outcome === "refusal" && (typeof coverage !== "string" || !/^n\/a:[^:]+$/.test(coverage))) {
    fail(
      "scenario_invalid",
      `${scenario.id}: a row that refuses instead of falling back must record its break-glass coverage as n/a:<reason>`,
      { host_fallback_coverage: coverage },
    );
  }
  if (typeof reason !== "string" || reason.length === 0) {
    fail("scenario_invalid", `${scenario.id}: the declared dead-transport outcome needs a reason`);
  }
  return {
    outcome,
    refusalNames,
    hostFallbackCoverage: typeof coverage === "string" ? coverage : undefined,
  };
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
 * family kills that transport, and what happens next is the row's own
 * declaration: the break-glass path announces itself in the ask it raises
 * ("AFT UNAVAILABLE") and banners the output it returns, so a row wearing the
 * other outcome's mark took the other path, whatever its file effects look
 * like.
 *
 * A fallback row only reaches that ask when the failure is one the product can
 * prove was never sent. On a transport that cannot say so, running the command
 * in the host would risk applying it twice, and the mark's ABSENCE is the
 * outcome under test: the rows that declare `refusal` assert exactly that, and
 * seeing the mark there is the defect. The configured denial is settled by the
 * host before the tool is dispatched at all, so a mark there would be a way
 * around the rule rather than the path under test.
 *
 * This half is asserted as soon as the host has finished, before the disk
 * proofs read their snapshots, because a row that took the wrong path makes
 * every later file claim unreadable.
 */
export function assertBashExecutionPathIdentity(
  scenario: ScenarioDefinition,
  observed: BashPermissionPathObservation,
): void {
  const family = bashPermissionFamily(scenario);
  if (!family) return;
  const details = { result_text: observed.resultText.slice(0, 600) };
  const fellBack = bashHostFallbackObserved(observed);
  if (family === "loop") {
    if (fellBack) {
      fail(
        "no_effect_observed",
        `${scenario.id}: the loop row fell back to host execution`,
        details,
      );
    }
    return;
  }
  const declared = bashDeadTransportDeclaration(scenario);
  if (declared.outcome === "host_fallback") {
    if (!fellBack) {
      fail(
        "no_effect_observed",
        `${scenario.id}: the dead transport raised no host-fallback ask, so bash refused the command instead of falling back`,
        details,
      );
    }
    return;
  }
  if (!fellBack) return;
  fail(
    "no_effect_observed",
    declared.outcome === "refusal"
      ? `${scenario.id}: bash fell back to host execution after a transport failure whose outcome is undetermined, which can run a command that was already sent`
      : `${scenario.id}: a rule-denied command raised the host-fallback ask, which would run it anyway`,
    details,
  );
}

/**
 * Assert that a refusing row refused, in the model's own view of the call.
 *
 * Two things make the refusal the right outcome rather than a silent nothing,
 * and both are read here. No ask was raised at all for the gated call: the
 * command is declined before either ask site, so neither the engine's
 * permission loop nor the break-glass prompt was reached, and the session's
 * standing answer never came into it. And the text the model received names
 * what was observed - the transport that failed, and the undetermined outcome
 * that makes re-running it unsafe - rather than a bare failure the agent
 * cannot act on.
 *
 * That the command did not RUN is proven elsewhere, and deliberately not from
 * this text: the row declares no disk effects, so the whole-tree snapshots
 * taken around the call refuse any change, and the task rows AFT keeps are
 * read once its processes have stopped.
 */
export function assertBashDeadTransportRefusal(
  scenario: ScenarioDefinition,
  observed: BashPermissionPathObservation,
): void {
  if (bashPermissionFamily(scenario) !== "fallback") return;
  const declared = bashDeadTransportDeclaration(scenario);
  if (declared.outcome !== "refusal") return;
  const callId = gatedCallId(scenario);
  if (!callId) {
    fail("scenario_invalid", `${scenario.id}: permission scenario has no subject tool call`);
  }
  const asked = observed.events.filter(
    (event) => event.type === "permission.asked" && asRecord(event.data.source)?.id === callId,
  );
  if (asked.length > 0) {
    fail(
      "no_effect_observed",
      `${scenario.id}: the refused call still raised a permission request, so it reached an ask site instead of being declined`,
      { observed: asked.map((event) => event.data) },
    );
  }
  for (const name of declared.refusalNames) {
    if (new RegExp(name).test(observed.resultText)) continue;
    fail(
      "no_effect_observed",
      `${scenario.id}: the refusal the model received does not name ${name}`,
      { result_text: observed.resultText.slice(0, 600) },
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
 * For a fallback row this is process-state evidence that AFT ran nothing, and
 * only that: the break-glass path runs the command in the host and leaves no
 * task row either, so what separates a refusal from a host execution is the
 * unchanged fixture tree, not this.
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
