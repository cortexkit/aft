/**
 * Cache-busting fix for prompt prefix cache eviction.
 *
 * PROBLEM: Every PromptInput we send to OpenCode (notifications, idle
 * wakeups, ignored messages) creates a new user message. OpenCode's
 * `createUserMessage` resolves variant relative to the chosen agent. If
 * we don't pass model/variant, defaults take over and bust the provider
 * prefix cache that the previous assistant turn warmed.
 *
 * APPROACH: Mirror what `opencode-xtra` does in production. Read recent
 * messages from `client.session.messages()`, prefer the most recent
 * assistant message, fall back to any role, MERGE across messages so
 * partial fields (e.g. agent set on user message but not assistant) are
 * filled in, and read BOTH the flat shape (`info.providerID`) used by
 * AssistantMessage and the nested shape (`info.model.providerID`) used
 * by UserMessage.
 *
 * IMPORTANT: This context is only meaningful for callers that DO trigger
 * LLM inference (e.g. background-bash idle wakes with `noReply: false`).
 * Callers using `noReply: true` (one-off ignored messages, warnings,
 * announcements) never trigger inference, so they don't need model or
 * variant — the model/variant pass-through there is unnecessary AND has
 * been observed to crash OpenCode under some configurations. Limit
 * model/variant pass-through to wake-style calls.
 */

export interface ResolvedPromptContext {
  agent?: string;
  model?: { providerID: string; modelID: string };
  variant?: string;
}

interface RawInfo {
  role?: string;
  agent?: string;
  variant?: string;
  providerID?: string;
  modelID?: string;
  model?: { providerID?: string; modelID?: string; variant?: string };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function extractMessages(response: unknown): unknown[] {
  if (Array.isArray(response)) return response;
  if (isRecord(response) && Array.isArray(response.data)) return response.data;
  return [];
}

function getRole(message: unknown): string | undefined {
  if (!isRecord(message) || !isRecord(message.info)) return undefined;
  return typeof message.info.role === "string" ? message.info.role : undefined;
}

function extractFromMessage(message: unknown): ResolvedPromptContext | null {
  if (!isRecord(message) || !isRecord(message.info)) return null;
  const info = message.info as RawInfo;
  const modelInfo = isRecord(info.model) ? info.model : undefined;

  const agent = typeof info.agent === "string" ? info.agent : undefined;
  const providerID =
    typeof modelInfo?.providerID === "string"
      ? modelInfo.providerID
      : typeof info.providerID === "string"
        ? info.providerID
        : undefined;
  const modelID =
    typeof modelInfo?.modelID === "string"
      ? modelInfo.modelID
      : typeof info.modelID === "string"
        ? info.modelID
        : undefined;
  const variant =
    typeof modelInfo?.variant === "string"
      ? modelInfo.variant
      : typeof info.variant === "string"
        ? info.variant
        : undefined;

  if (!agent && (!providerID || !modelID) && !variant) return null;
  const out: ResolvedPromptContext = {};
  if (agent) out.agent = agent;
  if (providerID && modelID) out.model = { providerID, modelID };
  if (variant) out.variant = variant;
  return out;
}

function mergeContexts(
  base: ResolvedPromptContext,
  patch: ResolvedPromptContext,
): ResolvedPromptContext {
  return {
    agent: base.agent ?? patch.agent,
    model: base.model ?? patch.model,
    variant: base.variant ?? patch.variant,
  };
}

function isComplete(ctx: ResolvedPromptContext): boolean {
  return Boolean(ctx.agent && ctx.model && ctx.variant);
}

/**
 * Read recent messages from the OpenCode session and resolve the most
 * recent assistant prompt context. Falls back to user messages if no
 * assistant has the field. Merges across messages so partial fields are
 * filled in. Returns null if no usable context is found.
 *
 * Mirrors `resolveSessionPromptParams` in `opencode-xtra` (the working
 * reference implementation).
 *
 * Bounded via `query.limit` — without it, OpenCode's legacy
 * `/session/{id}/message` endpoint hydrates the ENTIRE session. Large
 * sessions can carry 30k-45k messages / 100k+ parts and blow the host's
 * memory just to find the last assistant turn. We only need the most
 * recent prompt context, so 50 is plenty (the very last assistant
 * usually has everything; the merge fallback rarely needs more).
 *
 * Future v2 migration: once `@opencode-ai/sdk` exposes `client.v2.session.messages`
 * with projected shapes (user / assistant / agent-switched / model-switched / ...),
 * prefer that with `{ limit: 50, order: "desc" }` and fall back to this bounded
 * legacy call when v2 returns no items (older sessions aren't backfilled into v2).
 * See https://github.com/cortexkit/aft notes for tracking.
 */
const PROMPT_CONTEXT_MESSAGE_LIMIT = 50;

export async function resolvePromptContext(
  client: unknown,
  sessionId: string,
): Promise<ResolvedPromptContext | null> {
  if (!client || !sessionId) return null;
  const c = client as {
    session?: {
      messages?: (input: {
        path: { id: string };
        query?: { limit?: number };
      }) => Promise<{ data?: unknown[] } | unknown[]>;
    };
  };
  if (typeof c.session?.messages !== "function") return null;

  let messages: unknown[] = [];
  try {
    const response = await c.session.messages({
      path: { id: sessionId },
      query: { limit: PROMPT_CONTEXT_MESSAGE_LIMIT },
    });
    messages = extractMessages(response);
  } catch {
    return null;
  }
  if (messages.length === 0) return null;

  // Pass 1: prefer the most recent assistant, merge older assistants to
  // fill missing fields.
  let result: ResolvedPromptContext = {};
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    if (getRole(messages[i]) !== "assistant") continue;
    const ctx = extractFromMessage(messages[i]);
    if (!ctx) continue;
    result = mergeContexts(result, ctx);
    if (isComplete(result)) return result;
  }

  // Pass 2: fall back to any role (covers user messages, which carry
  // model nested under `info.model`).
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    const ctx = extractFromMessage(messages[i]);
    if (!ctx) continue;
    result = mergeContexts(result, ctx);
    if (isComplete(result)) return result;
  }

  if (!result.agent && !result.model && !result.variant) return null;
  return result;
}

// --- Compatibility shim for any older caller still using this name ---

export interface LastAssistantModel {
  providerID: string;
  modelID: string;
  variant?: string;
}

/** @deprecated Use {@link resolvePromptContext} which also returns agent. */
export async function getLastAssistantModel(
  client: unknown,
  sessionId: string,
): Promise<LastAssistantModel | null> {
  const ctx = await resolvePromptContext(client, sessionId);
  if (!ctx?.model) return null;
  return {
    providerID: ctx.model.providerID,
    modelID: ctx.model.modelID,
    ...(ctx.variant ? { variant: ctx.variant } : {}),
  };
}
