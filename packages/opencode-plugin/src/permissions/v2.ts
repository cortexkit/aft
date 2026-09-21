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

/**
 * Raised when the prompt was shown and no answer ever came back.
 *
 * Like the unavailable-prompt refusal this keeps its own text, because that
 * text is the only place the user learns that a prompt was raised on their
 * behalf and went unanswered. Never an allow: an answer nobody gave is not
 * permission.
 */
export class PermissionPromptUnansweredError extends Error {
  override readonly name = "PermissionPromptUnansweredError";
}

/** Raised when the host's rules for this call could not be read at all. */
export class PermissionRulesUnavailableError extends Error {
  override readonly name = "PermissionRulesUnavailableError";
}

/**
 * How long a raised prompt may stay unanswered before the call is refused.
 *
 * A prompt is a question for a person, and a person may be away from the
 * machine: ten minutes is well beyond the minutes a real answer takes, so an
 * answer that is coming still arrives in time. The bound exists for the answer
 * that is not coming, which used to hold the tool call open for the life of the
 * session.
 */
export const V2_PROMPT_REPLY_TIMEOUT_MS = 10 * 60_000;

/**
 * How long AFT waits for its event subscription to start delivering before it
 * raises the request anyway.
 *
 * OpenCode greets a new subscriber immediately, so this is normally over in a
 * millisecond or two on a loopback connection. It is a bound rather than an
 * unconditional wait so a host that greets nobody cannot stop prompts from
 * being raised at all; a request raised without a confirmed subscription is
 * still bounded by the reply timeout, and refuses rather than allows.
 */
export const V2_PROMPT_LISTEN_TIMEOUT_MS = 5_000;

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

/**
 * The refusal for a prompt that was raised and never answered.
 *
 * It states what was observed rather than a verdict, because the difference
 * matters to whoever reads it: the operation was not denied by anyone, the
 * answer simply never reached AFT, and nothing was permitted in the meantime.
 */
function promptUnansweredMessage(
  request: V2PermissionRequest,
  decision: V2PermissionDecision,
  observed: string,
): string {
  const resource = decision.resource ?? "*";
  return (
    `The ${JSON.stringify(request.permission)} operation was refused: this session's permission ` +
    `rules resolve it to "ask" for ${JSON.stringify(resource)}, AFT raised a prompt and received ` +
    `no reply (${observed}), so nothing was permitted. Answer the prompt and run the operation ` +
    "again, or run it without a prompt by adding " +
    `{"action": ${JSON.stringify(request.permission)}, "resource": ${JSON.stringify(resource)}, ` +
    '"effect": "allow"} to the "permissions" list of the agent or session in your OpenCode config.'
  );
}

/** Render a wait bound the way a refusal quotes it back. */
function waitDuration(ms: number): string {
  if (ms >= 60_000 && ms % 60_000 === 0) return `${ms / 60_000}m`;
  if (ms >= 1_000 && ms % 1_000 === 0) return `${ms / 1_000}s`;
  return `${ms}ms`;
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

/**
 * One pull from the event stream, with its failure folded into the value.
 *
 * A pull is started before anyone is ready to await it, so it must never be a
 * promise that can reject while unobserved.
 */
type StreamPull =
  | { readonly kind: "event"; readonly result: IteratorResult<unknown> }
  | { readonly kind: "failed"; readonly error: unknown };

function pull(iterator: AsyncIterator<unknown>): Promise<StreamPull> {
  return iterator.next().then(
    (result) => ({ kind: "event", result }) as const,
    (error) => ({ kind: "failed", error }) as const,
  );
}

interface Countdown {
  readonly expired: Promise<void>;
  cancel(): void;
}

function countdown(ms: number): Countdown {
  let handle: ReturnType<typeof setTimeout> | undefined;
  const expired = new Promise<void>((resolve) => {
    handle = setTimeout(resolve, ms);
  });
  return {
    expired,
    cancel: () => {
      if (handle !== undefined) clearTimeout(handle);
    },
  };
}

interface Cancellation {
  readonly cancelled: Promise<void>;
  dispose(): void;
}

function cancellation(signal: AbortSignal | undefined): Cancellation {
  if (!signal) return { cancelled: new Promise<void>(() => {}), dispose: () => {} };
  if (signal.aborted) return { cancelled: Promise.resolve(), dispose: () => {} };
  let listener: (() => void) | undefined;
  const cancelled = new Promise<void>((resolve) => {
    listener = () => resolve();
    signal.addEventListener("abort", listener, { once: true });
  });
  return {
    cancelled,
    dispose: () => {
      if (listener) signal.removeEventListener("abort", listener);
    },
  };
}

/**
 * Open the event stream and wait until it is actually delivering.
 *
 * The generated client's `event.subscribe()` hands back a lazy async generator:
 * holding it, or even taking its iterator, performs no I/O at all, and the HTTP
 * request that attaches the subscription is only made by the first `next()`.
 * The host forwards events to whoever is attached when they are emitted and
 * keeps no backlog, so a subscription that attaches after a request has been
 * announced can never be told how that request was answered.
 *
 * Pulling one event is therefore what subscribing means here. OpenCode greets
 * every new subscriber, so the pull settles as soon as the stream is live. The
 * pulled event is not thrown away: it is handed to the wait as its first item,
 * so a greeting and a real event are treated the same way and nothing can be
 * swallowed by the handshake.
 */
async function startListening(
  client: V2PermissionClient,
  listenTimeoutMs: number,
): Promise<
  | { readonly iterator: AsyncIterator<unknown>; readonly first: Promise<StreamPull> }
  | { readonly unavailable: string }
> {
  let iterator: AsyncIterator<unknown>;
  try {
    const subscription = await client.event.subscribe();
    iterator = subscription.stream[Symbol.asyncIterator]();
  } catch (error) {
    return { unavailable: `subscribing to the event stream failed: ${detail(error)}` };
  }

  const first = pull(iterator);
  const ready = countdown(listenTimeoutMs);
  try {
    const settled = await Promise.race([first, ready.expired.then(() => undefined)]);
    if (settled?.kind === "failed") {
      await closeStream(iterator);
      return { unavailable: `reading the event stream failed: ${detail(settled.error)}` };
    }
  } finally {
    ready.cancel();
  }
  return { iterator, first };
}

interface ReplyWaitLimits {
  readonly replyTimeoutMs: number;
  readonly signal?: AbortSignal;
}

/**
 * Wait for this request's answer, for as long as the bound allows.
 *
 * `first` is the pull that made the subscription live; it is examined like any
 * other event so the handshake cannot hide one.
 */
async function waitForReply(
  iterator: AsyncIterator<unknown>,
  first: Promise<StreamPull>,
  sessionID: string,
  requestID: string,
  limits: ReplyWaitLimits,
): Promise<void> {
  const deadline = countdown(limits.replyTimeoutMs);
  const abort = cancellation(limits.signal);
  let pending: Promise<StreamPull> | undefined = first;
  try {
    while (true) {
      const next = pending ?? pull(iterator);
      pending = undefined;
      const settled = await Promise.race([
        next,
        deadline.expired.then(() => ({ kind: "unanswered" }) as const),
        abort.cancelled.then(() => ({ kind: "cancelled" }) as const),
      ]);
      if (settled.kind === "unanswered") {
        throw new PermissionPromptUnansweredError(
          `waited ${waitDuration(limits.replyTimeoutMs)} after raising it`,
        );
      }
      if (settled.kind === "cancelled") {
        throw new PermissionPromptUnansweredError(
          "the operation was cancelled while the prompt was still open",
        );
      }
      if (settled.kind === "failed") {
        throw new PermissionPromptUnansweredError(
          `reading the event stream failed: ${detail(settled.error)}`,
        );
      }
      if (settled.result.done) {
        throw new PermissionPromptUnansweredError("the event stream ended before a reply arrived");
      }
      const event = repliedEvent(settled.result.value);
      if (!event || event.sessionID !== sessionID || event.requestID !== requestID) continue;
      if (event.reply === "reject") throw new PermissionRejectedError("Permission denied.");
      return;
    }
  } finally {
    deadline.cancel();
    abort.dispose();
    await closeStream(iterator);
  }
}

/**
 * Put the question to the user through OpenCode's own permission service.
 *
 * Listening starts before `permission.create`, and listening means a stream
 * that has already delivered something — a headless client answers within a
 * millisecond or two of the request being announced, and an answer delivered
 * before the subscription attaches is gone for good. The create response still
 * handles saved-rule allows and configured denies immediately; only an `ask`
 * waits for its matching reply, and that wait is bounded.
 *
 * `request.metadata` is forwarded unchanged, which is what gives an edit-class
 * request its `diff` and makes the host render its built-in patch view.
 */
async function raisePrompt(
  client: V2PermissionClient,
  request: V2PermissionRequest,
  decision: V2PermissionDecision,
  context: V2ExecutionContext,
  limits: ResolvedPromptLimits,
): Promise<void> {
  const sessionID = requiredContextID(context, "sessionID");
  const messageID = requiredContextID(context, "messageID");
  const id = requiredContextID(context, "id");
  const listening = await startListening(client, limits.listenTimeoutMs);
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

    // Only an answer we could hear is worth raising a prompt for. A settled
    // effect above needed no stream, so this refusal is deliberately reached
    // after the create call rather than before it.
    if ("unavailable" in listening) {
      throw new PermissionPromptUnavailableError(
        promptUnavailableMessage(request, decision, listening.unavailable),
      );
    }

    streamClaimed = true;
    try {
      await waitForReply(listening.iterator, listening.first, sessionID, result.id, limits);
    } catch (error) {
      if (!(error instanceof PermissionPromptUnansweredError)) throw error;
      throw new PermissionPromptUnansweredError(
        promptUnansweredMessage(request, decision, error.message),
      );
    }
  } finally {
    if (!streamClaimed && "iterator" in listening) await closeStream(listening.iterator);
  }
}

/** The wait bounds and cancellation one prompt runs under. */
export interface V2PromptLimits {
  readonly replyTimeoutMs?: number;
  readonly listenTimeoutMs?: number;
  /** Ends the wait when the tool call it belongs to is cancelled. */
  readonly signal?: AbortSignal;
}

interface ResolvedPromptLimits extends ReplyWaitLimits {
  readonly listenTimeoutMs: number;
}

/**
 * The tool call's own cancellation signal, when its context carries one.
 *
 * The runtime object a V2 tool executes against exposes the host's abort signal
 * as `abort`; the plain execution contexts AFT's own fixtures build do not have
 * one. Reading it defensively is what keeps a cancelled call from leaving a
 * prompt wait running behind it, without requiring every caller to thread the
 * signal through by hand.
 */
function contextAbortSignal(context: V2ExecutionContext): AbortSignal | undefined {
  const candidate = (context as { abort?: unknown }).abort;
  return candidate instanceof AbortSignal ? candidate : undefined;
}

function resolveLimits(context: V2ExecutionContext, limits: V2PromptLimits): ResolvedPromptLimits {
  return {
    replyTimeoutMs: limits.replyTimeoutMs ?? V2_PROMPT_REPLY_TIMEOUT_MS,
    listenTimeoutMs: limits.listenTimeoutMs ?? V2_PROMPT_LISTEN_TIMEOUT_MS,
    signal: limits.signal ?? contextAbortSignal(context),
  };
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
  limits: V2PromptLimits = {},
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
  await raisePrompt(channel.client, request, decision, context, resolveLimits(context, limits));
}
