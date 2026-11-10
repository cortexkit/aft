import { LLMock } from "@copilotkit/aimock";

export interface AimockHandle {
  url: string;
  close: () => Promise<void>;
  registerToolCallFixture(opts: {
    predicate: (request: unknown) => boolean;
    toolCalls: Array<{ name: string; arguments: Record<string, unknown> }>;
    followupText?: string;
  }): void;
  registerTextFixture(opts: { predicate: (request: unknown) => boolean; content: string }): void;
  /**
   * Like registerToolCallFixture for one call, but while the call's result
   * contains `retryWhile` the mock model calls the tool again (up to
   * `attempts` calls in total) before answering `followupText`. For tools that
   * give a designed "retry shortly" reply while AFT is still starting up.
   */
  registerRetryingToolCallFixture(opts: {
    toolCall: { name: string; arguments: Record<string, unknown> };
    retryWhile: string;
    attempts: number;
    followupText: string;
  }): void;
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

function lastToolCallId(request: unknown): string | undefined {
  const messages = asRecord(request)?.messages;
  const messageList = Array.isArray(messages) ? messages : [];
  const toolMessage = asRecord(messageList.at(-1));
  if (toolMessage?.role !== "tool") return undefined;
  return typeof toolMessage?.tool_call_id === "string" ? toolMessage.tool_call_id : undefined;
}

function lastToolContent(request: unknown): string {
  const messages = asRecord(request)?.messages;
  const messageList = Array.isArray(messages) ? messages : [];
  const content = asRecord(messageList.at(-1))?.content;
  if (typeof content === "string") return content;
  return Array.isArray(content)
    ? content
        .map((part) => {
          const text = asRecord(part)?.text;
          return typeof text === "string" ? text : "";
        })
        .join("\n")
    : "";
}

export async function startAimock(port = 0): Promise<AimockHandle> {
  const mock = new LLMock({ port });
  await mock.start();
  let fixtureIndex = 0;

  return {
    url: mock.url,
    close: () => mock.stop(),
    registerToolCallFixture(opts) {
      const callIds = opts.toolCalls.map(() => `call_aft_${++fixtureIndex}`);
      if (opts.followupText) {
        for (const callId of callIds) {
          mock.on(
            { predicate: (request: unknown) => lastToolCallId(request) === callId },
            {
              content: opts.followupText,
            },
          );
        }
      }

      mock.on(
        {
          predicate: (request: unknown) =>
            lastToolCallId(request) === undefined && opts.predicate(request),
        },
        {
          toolCalls: opts.toolCalls.map((toolCall, index) => ({
            id: callIds[index],
            name: toolCall.name,
            arguments: toolCall.arguments,
          })),
        },
      );
    },
    registerTextFixture(opts) {
      mock.on({ predicate: opts.predicate }, { content: opts.content });
    },
    registerRetryingToolCallFixture(opts) {
      const callIds = Array.from({ length: opts.attempts }, () => `call_aft_${++fixtureIndex}`);
      const call = (id: string) => ({
        toolCalls: [{ id, name: opts.toolCall.name, arguments: opts.toolCall.arguments }],
      });
      callIds.forEach((callId, index) => {
        const next = callIds[index + 1];
        if (next) {
          // Registered before the follow-up below, so a retryable reply is
          // answered with the next call rather than the closing text.
          mock.on(
            {
              predicate: (request: unknown) =>
                lastToolCallId(request) === callId &&
                lastToolContent(request).includes(opts.retryWhile),
            },
            call(next),
          );
        }
        mock.on(
          { predicate: (request: unknown) => lastToolCallId(request) === callId },
          { content: opts.followupText },
        );
      });
      mock.on(
        { predicate: (request: unknown) => lastToolCallId(request) === undefined },
        call(callIds[0]),
      );
    },
  };
}
