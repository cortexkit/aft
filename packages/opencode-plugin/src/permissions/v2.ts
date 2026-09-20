import { Effect } from "effect";

import type { V2ExecutionContext, V2PermissionRequest } from "../tools/definitions/v2.js";
import type { V2PromptChannel } from "./v2-service.js";

export type V2PermissionEffect = "allow" | "deny" | "ask";

/** One ordered `{action, resource, effect}` entry of an OpenCode permission ruleset. */
export interface V2PermissionRule {
  readonly action: string;
  readonly resource: string;
  readonly effect: V2PermissionEffect;
}

export interface V2AgentRecord {
  readonly permissions?: readonly V2PermissionRule[];
}

export interface V2SessionRecord {
  readonly agent?: string;
  readonly permissions?: readonly V2PermissionRule[];
}

/**
 * The two plugin domains AFT reads to learn the host's configured answer.
 *
 * OpenCode 2 hands a plugin `agent` and `session` domains whose records carry
 * the same ordered permission rulesets the host itself evaluates, so a tool can
 * resolve "allow", "deny", or "ask" without a round trip to the user.
 */
export interface V2PermissionHostContext {
  readonly agent: {
    get(input: { agentID: string }): Effect.Effect<{ data: V2AgentRecord }, unknown>;
  };
  readonly session: {
    get(input: { sessionID: string }): Effect.Effect<V2SessionRecord | undefined, unknown>;
  };
}

export type V2PermissionReply = "once" | "always" | "reject";

export interface V2PermissionCreateInput {
  sessionID: string;
  action: string;
  resources: string[];
  save: string[];
  metadata: Record<string, unknown>;
  source: {
    type: "tool";
    messageID: string;
    id: string;
  };
}

interface V2PermissionCreateResult {
  id: string;
  effect: V2PermissionEffect;
}

interface V2PermissionRepliedEvent {
  sessionID: string;
  requestID: string;
  reply: V2PermissionReply;
}

interface V2EventSubscription {
  stream: AsyncIterable<unknown>;
}

/** The two service calls a permission prompt needs, in the shape AFT sends them. */
export interface V2PermissionClient {
  permission: {
    create(
      input: V2PermissionCreateInput,
    ): Promise<V2PermissionCreateResult | { data?: V2PermissionCreateResult; error?: unknown }>;
  };
  event: {
    subscribe(): Promise<V2EventSubscription>;
  };
}

export class PermissionDeniedError extends Error {
  override readonly name = "PermissionDeniedError";
}

export class PermissionRejectedError extends Error {
  override readonly name = "PermissionRejectedError";
}

/**
 * Raised when the rules resolve to "ask" and the service that shows the prompt
 * could not be reached.
 *
 * Deliberately not named like a rule denial: the caller-side classifier rewrites
 * recognised denial classes into a generic "denied by rule" sentence, and this
 * refusal has to keep its own text because that text names the way out.
 */
export class PermissionPromptUnavailableError extends Error {
  override readonly name = "PermissionPromptUnavailableError";
}

/** Raised when the host's rules for this call could not be read at all. */
export class PermissionRulesUnavailableError extends Error {
  override readonly name = "PermissionRulesUnavailableError";
}

/**
 * OpenCode's permission pattern matcher, kept byte-compatible with the host.
 *
 * Both the action and the resource are matched with it, `*` spans any run of
 * characters, `?` spans one, and a pattern ending in " *" also matches the
 * bare prefix so a `bash` rule like "git push *" still covers "git push".
 */
function wildcardMatch(input: string, pattern: string): boolean {
  const normalized = input.replaceAll("\\", "/");
  let escaped = pattern
    .replaceAll("\\", "/")
    .replaceAll(/[.+^${}()|[\]\\]/g, "\\$&")
    .replaceAll(/\*/g, ".*")
    .replaceAll(/\?/g, ".");
  if (escaped.endsWith(" .*")) escaped = `${escaped.slice(0, -3)}( .*)?`;
  return new RegExp(`^${escaped}$`, process.platform === "win32" ? "si" : "s").test(normalized);
}

/**
 * Last matching rule wins; an unmatched action falls through to "ask".
 *
 * Reverse iteration rather than `findLast` because this package targets ES2022.
 */
function matchingRule(
  action: string,
  resource: string,
  rules: readonly V2PermissionRule[],
): V2PermissionRule {
  for (let index = rules.length - 1; index >= 0; index -= 1) {
    const rule = rules[index];
    if (rule && wildcardMatch(action, rule.action) && wildcardMatch(resource, rule.resource)) {
      return rule;
    }
  }
  return { action, resource: "*", effect: "ask" };
}

export interface V2PermissionDecision {
  readonly effect: V2PermissionEffect;
  /** The resource that decided the outcome, absent when every resource allowed. */
  readonly resource?: string;
  /** The rule that decided the outcome, absent when every resource allowed. */
  readonly rule?: V2PermissionRule;
}

/**
 * Resolve one AFT permission request against a host ruleset.
 *
 * Mirrors OpenCode's own evaluation: any denied resource denies the whole
 * request, otherwise any resource that still wants a prompt makes the request
 * an "ask", and only a request whose resources are all allowed is allowed.
 *
 * One deliberate difference: OpenCode also folds in the user's saved "always"
 * grants before the allow/ask split, and those are not readable from a plugin.
 * A saved grant therefore reads as "ask" here, which costs one round trip to
 * the service: the host applies the grant when the request reaches it and
 * answers "allow" without showing anything.
 */
export function decidePermission(
  request: V2PermissionRequest,
  rules: readonly V2PermissionRule[],
): V2PermissionDecision {
  for (const resource of request.patterns) {
    const rule = matchingRule(request.permission, resource, rules);
    if (rule.effect === "deny") return { effect: "deny", resource, rule };
  }
  for (const resource of request.patterns) {
    const rule = matchingRule(request.permission, resource, rules);
    if (rule.effect === "ask") return { effect: "ask", resource, rule };
  }
  return { effect: "allow" };
}

function detail(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function runHostEffect<A>(effect: Effect.Effect<A, unknown>): Promise<A> {
  return await Effect.runPromise(effect as Effect.Effect<A, never>);
}

async function sessionRecord(
  host: V2PermissionHostContext,
  sessionID: string | undefined,
): Promise<V2SessionRecord | undefined> {
  if (!sessionID) return undefined;
  try {
    return await runHostEffect(host.session.get({ sessionID }));
  } catch (error) {
    throw new PermissionRulesUnavailableError(
      `reading session ${JSON.stringify(sessionID)} failed: ${detail(error)}`,
    );
  }
}

async function agentRules(
  host: V2PermissionHostContext,
  agentID: string,
): Promise<readonly V2PermissionRule[]> {
  let record: V2AgentRecord;
  try {
    record = (await runHostEffect(host.agent.get({ agentID }))).data;
  } catch (error) {
    throw new PermissionRulesUnavailableError(
      `reading agent ${JSON.stringify(agentID)} failed: ${detail(error)}`,
    );
  }
  if (!record.permissions) {
    throw new PermissionRulesUnavailableError(
      `agent ${JSON.stringify(agentID)} reported no permission rules`,
    );
  }
  return record.permissions;
}

/**
 * Read the ordered ruleset OpenCode would evaluate for this tool call.
 *
 * The agent's rules come first and the session's own overrides are appended
 * after them, which is the order the host uses and the reason a session
 * override wins: evaluation takes the last matching rule.
 */
async function configuredRules(
  host: V2PermissionHostContext,
  context: V2ExecutionContext,
): Promise<readonly V2PermissionRule[]> {
  const session = await sessionRecord(host, context.sessionID);
  const agentID = context.agent ?? session?.agent;
  if (!agentID) {
    throw new PermissionRulesUnavailableError(
      "the tool call carried no agent identifier and its session named none",
    );
  }
  return [...(await agentRules(host, agentID)), ...(session?.permissions ?? [])];
}

function describeRule(rule: V2PermissionRule | undefined): string {
  if (!rule) return "no matching rule";
  return `${JSON.stringify(rule.action)} on ${JSON.stringify(rule.resource)} is ${JSON.stringify(rule.effect)}`;
}

function deniedMessage(request: V2PermissionRequest, decision: V2PermissionDecision): string {
  return (
    `Permission denied: this session's permission rules deny ${JSON.stringify(request.permission)}` +
    `${decision.resource === undefined ? "" : ` for ${JSON.stringify(decision.resource)}`}` +
    ` (${describeRule(decision.rule)}).`
  );
}

function promptUnavailableMessage(
  request: V2PermissionRequest,
  decision: V2PermissionDecision,
  observed: string,
): string {
  const resource = decision.resource ?? "*";
  return (
    `The ${JSON.stringify(request.permission)} operation needs interactive approval: this ` +
    `session's permission rules resolve it to "ask" for ${JSON.stringify(resource)}, and AFT ` +
    `could not reach the OpenCode service to raise a prompt (${observed}). Start OpenCode's ` +
    "background service and retry, or run it without a prompt by adding " +
    `{"action": ${JSON.stringify(request.permission)}, "resource": ${JSON.stringify(resource)}, ` +
    '"effect": "allow"} to the "permissions" list of the agent or session in your OpenCode config.'
  );
}

function requiredContextID(
  context: V2ExecutionContext,
  key: "sessionID" | "messageID" | "id",
): string {
  const value = context[key];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`V2 permission request requires context.${key}`);
  }
  return value;
}

function createResult(
  response: V2PermissionCreateResult | { data?: V2PermissionCreateResult; error?: unknown },
): V2PermissionCreateResult {
  if ("id" in response && "effect" in response) return response;
  if (response.error !== undefined) {
    throw response.error instanceof Error
      ? response.error
      : new Error(`Permission request failed: ${String(response.error)}`);
  }
  if (!response.data) throw new Error("Permission request returned no result");
  return response.data;
}

/**
 * Recognise a "the user answered" event on the service's event stream.
 *
 * The payload is read from `data`, which is where OpenCode 2 puts an event's
 * body, and the whole envelope is unwrapped from `payload` first for transports
 * that nest it.
 */
function repliedEvent(event: unknown): V2PermissionRepliedEvent | undefined {
  if (!event || typeof event !== "object") return undefined;
  const envelope = event as { payload?: unknown };
  const candidate = (envelope.payload ?? event) as {
    type?: unknown;
    data?: unknown;
  };
  if (candidate.type !== "permission.replied") return undefined;
  if (!candidate.data || typeof candidate.data !== "object") return undefined;
  return candidate.data as V2PermissionRepliedEvent;
}

async function closeStream(iterator: AsyncIterator<unknown>): Promise<void> {
  if (typeof iterator.return === "function") await iterator.return();
}

async function waitForReply(
  stream: AsyncIterable<unknown>,
  sessionID: string,
  requestID: string,
): Promise<void> {
  const iterator = stream[Symbol.asyncIterator]();
  try {
    while (true) {
      const next = await iterator.next();
      if (next.done) throw new Error("Permission event stream ended before a reply arrived");
      const event = repliedEvent(next.value);
      if (!event || event.sessionID !== sessionID || event.requestID !== requestID) continue;
      if (event.reply === "reject") throw new PermissionRejectedError("Permission denied.");
      return;
    }
  } finally {
    await closeStream(iterator);
  }
}

/**
 * Put the question to the user through OpenCode's own permission service.
 *
 * Subscribing before `permission.create` prevents a fast headless reply from
 * racing past the event listener. The create response handles saved-rule allows
 * and configured denies immediately; only an `ask` waits for its matching reply.
 *
 * `request.metadata` is forwarded unchanged, which is what gives an edit-class
 * request its `diff` and makes the host render its built-in patch view.
 */
async function raisePrompt(
  client: V2PermissionClient,
  request: V2PermissionRequest,
  context: V2ExecutionContext,
): Promise<void> {
  const sessionID = requiredContextID(context, "sessionID");
  const messageID = requiredContextID(context, "messageID");
  const id = requiredContextID(context, "id");
  const subscription = await client.event.subscribe();
  const iterator = subscription.stream[Symbol.asyncIterator]();
  let streamClaimed = false;

  try {
    const result = createResult(
      await client.permission.create({
        sessionID,
        action: request.permission,
        resources: [...request.patterns],
        save: [...request.always],
        metadata: { ...request.metadata },
        source: { type: "tool", messageID, id },
      }),
    );

    if (result.effect === "allow") return;
    if (result.effect === "deny") throw new PermissionDeniedError("Permission denied.");

    streamClaimed = true;
    await waitForReply({ [Symbol.asyncIterator]: () => iterator }, sessionID, result.id);
  } finally {
    if (!streamClaimed) await closeStream(iterator);
  }
}

/**
 * Answer a V2 tool's permission request.
 *
 * The host's configured rules are evaluated first, so anything the user has
 * already permitted — reading a file inside the project under OpenCode's
 * default allow-everything ruleset, for instance — runs with no round trip,
 * and a denial fails naming the rule that produced it. Only a rule that asks
 * costs a call to the service, which raises the real prompt.
 */
export async function requestPermission(
  host: V2PermissionHostContext,
  request: V2PermissionRequest,
  context: V2ExecutionContext,
  prompt: V2PromptChannel,
): Promise<void> {
  let rules: readonly V2PermissionRule[];
  try {
    rules = await configuredRules(host, context);
  } catch (error) {
    if (!(error instanceof PermissionRulesUnavailableError)) throw error;
    throw new PermissionRulesUnavailableError(
      `The ${JSON.stringify(request.permission)} operation was refused because AFT could not ` +
        `read this session's OpenCode permission rules (${error.message}), so it cannot tell ` +
        "whether the operation is permitted.",
    );
  }

  const decision = decidePermission(request, rules);
  if (decision.effect === "allow") return;
  if (decision.effect === "deny") throw new PermissionDeniedError(deniedMessage(request, decision));

  const channel = await prompt.client();
  if (!channel.client) {
    throw new PermissionPromptUnavailableError(
      promptUnavailableMessage(request, decision, channel.unavailable),
    );
  }
  await raisePrompt(channel.client, request, context);
}
