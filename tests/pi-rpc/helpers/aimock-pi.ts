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
  };
}
