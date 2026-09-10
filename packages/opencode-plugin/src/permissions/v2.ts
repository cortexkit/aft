import type { V2ExecutionContext, V2PermissionRequest } from "../tools/definitions/v2.js";

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
  effect: "allow" | "deny" | "ask";
}

interface V2PermissionRepliedEvent {
  type: "permission.replied";
  properties: {
    sessionID: string;
    requestID: string;
    reply: V2PermissionReply;
  };
}

interface V2EventSubscription {
  stream: AsyncIterable<unknown>;
}

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

export interface V2PermissionHostContext {
  client: V2PermissionClient;
}

export class PermissionDeniedError extends Error {
  override readonly name = "PermissionDeniedError";
}

export class PermissionRejectedError extends Error {
  override readonly name = "PermissionRejectedError";
}

function requiredContextID(
  context: V2ExecutionContext,
  key: "sessionID" | "messageID" | "id",
): string {
  const value = (context as V2ExecutionContext & { id?: string })[key];
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

function repliedEvent(event: unknown): V2PermissionRepliedEvent | undefined {
  if (!event || typeof event !== "object") return undefined;
  const envelope = event as { payload?: unknown };
  const candidate = (envelope.payload ?? event) as {
    type?: unknown;
    properties?: unknown;
  };
  if (candidate.type !== "permission.replied") return undefined;
  if (!candidate.properties || typeof candidate.properties !== "object") return undefined;
  return candidate as V2PermissionRepliedEvent;
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
      if (
        !event ||
        event.properties.sessionID !== sessionID ||
        event.properties.requestID !== requestID
      ) {
        continue;
      }
      if (event.properties.reply === "reject") {
        throw new PermissionRejectedError("Permission denied.");
      }
      return;
    }
  } finally {
    await closeStream(iterator);
  }
}

/**
 * Ask OpenCode's Location-scoped permission service for a V2 tool operation.
 *
 * Subscribing before `permission.create` prevents a fast headless reply from
 * racing past the event listener. The create response handles saved-rule allows
 * and configured denies immediately; only an `ask` waits for its matching reply.
 */
export async function requestPermission(
  host: V2PermissionHostContext,
  request: V2PermissionRequest,
  context: V2ExecutionContext,
): Promise<void> {
  const sessionID = requiredContextID(context, "sessionID");
  const messageID = requiredContextID(context, "messageID");
  const id = requiredContextID(context, "id");
  const subscription = await host.client.event.subscribe();
  const iterator = subscription.stream[Symbol.asyncIterator]();
  let streamClaimed = false;

  try {
    const result = createResult(
      await host.client.permission.create({
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
