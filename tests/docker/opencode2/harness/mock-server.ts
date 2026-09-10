import { createRequire } from "node:module";
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

import type {
  RecordedMockExchange,
  ScenarioDefinition,
  ScriptedTurn,
  ToolCallPlan,
} from "./types.js";
import { fail } from "./errors.js";
import { asRecord } from "./util.js";

interface LLMockLike {
  url: string;
  on(
    matcher: { sequenceIndex?: number; predicate?: (request: unknown) => boolean },
    response: unknown,
    options?: unknown,
  ): void;
  onMessage(pattern: string | RegExp, response: unknown): void;
  getRequests(): unknown[];
  start(): Promise<void>;
  stop(): Promise<void>;
}

interface LLMockConstructor {
  new (options: { port: number }): LLMockLike;
}

export interface DeterministicMockHooks {
  beforeTurn?: (turn: ScriptedTurn, request: unknown) => void | Promise<void>;
  afterRequest?: (exchange: RecordedMockExchange, turn?: ScriptedTurn) => void | Promise<void>;
  afterResponse?: (exchange: RecordedMockExchange) => void | Promise<void>;
}

const TURN_PLACEHOLDER = /\{\{(permission_id|session_id|task_id)(?::([^{}]+))?\}\}/g;

function materializeValue(value: unknown, values: Readonly<Record<string, string>>): unknown {
  if (typeof value === "string") {
    return value.replace(
      TURN_PLACEHOLDER,
      (placeholder, kind: string, qualifier: string | undefined) => {
        const key = qualifier ? `${kind}:${qualifier}` : kind;
        const replacement = values[key];
        if (!replacement) {
          fail("scenario_invalid", `scripted turn placeholder has no value: ${placeholder}`);
        }
        return replacement;
      },
    );
  }
  if (Array.isArray(value)) return value.map((entry) => materializeValue(entry, values));
  const record = asRecord(value);
  if (!record) return value;
  return Object.fromEntries(
    Object.entries(record).map(([key, entry]) => [key, materializeValue(entry, values)]),
  );
}

export function materializeTurnPlaceholders(
  turn: ScriptedTurn,
  values: Readonly<Record<string, string>>,
): void {
  if (turn.response.kind === "text") {
    turn.response.content = materializeValue(turn.response.content, values) as string;
    return;
  }
  for (const call of turn.response.calls) {
    call.arguments = materializeValue(call.arguments, values) as Record<string, unknown>;
  }
}

function mockResponse(turn: ScriptedTurn): Record<string, unknown> {
  if (turn.response.kind === "text") return { content: turn.response.content };
  return {
    toolCalls: turn.response.calls.map((call) => ({
      id: call.id,
      name: call.name,
      arguments: JSON.stringify(call.arguments),
    })),
  };
}

export async function observeThenRespond(
  turn: ScriptedTurn,
  request: unknown,
  index: number,
  hooks: DeterministicMockHooks,
): Promise<{ exchange: RecordedMockExchange; response: Record<string, unknown> }> {
  await hooks.beforeTurn?.(turn, request);
  const exchange: RecordedMockExchange = {
    index,
    label: turn.label,
    request,
    response: undefined,
    observed_at: new Date().toISOString(),
  };
  await hooks.afterRequest?.(exchange, turn);
  const response = mockResponse(turn);
  exchange.response = response;
  await hooks.afterResponse?.(exchange);
  return { exchange, response };
}

export function isTitleGenerationRequest(request: unknown): boolean {
  const messages = asRecord(request)?.messages;
  if (!Array.isArray(messages)) return false;
  return messages.some((value) => {
    const message = asRecord(value);
    return (
      message?.role === "system" &&
      typeof message.content === "string" &&
      message.content.includes("You are a title generator")
    );
  });
}

export class DeterministicScenarioMock {
  readonly scenario: ScenarioDefinition;
  readonly turnLogPath: string;
  readonly exchanges: RecordedMockExchange[] = [];
  readonly hooks: DeterministicMockHooks;
  #mock?: LLMockLike;

  constructor(
    scenario: ScenarioDefinition,
    turnLogPath: string,
    hooks: DeterministicMockHooks = {},
  ) {
    this.scenario = scenario;
    this.turnLogPath = turnLogPath;
    this.hooks = hooks;
  }

  get url(): string {
    if (!this.#mock) throw new Error("deterministic mock has not started");
    return this.#mock.url;
  }

  get requests(): unknown[] {
    return this.#mock?.getRequests() ?? [];
  }

  async start(port = 0): Promise<void> {
    mkdirSync(dirname(this.turnLogPath), { recursive: true });
    writeFileSync(this.turnLogPath, "");
    const require = createRequire(import.meta.url);
    const loaded = require("@copilotkit/aimock") as { LLMock?: LLMockConstructor };
    if (!loaded.LLMock) throw new Error("@copilotkit/aimock does not export LLMock");
    const mock = new loaded.LLMock({ port });
    this.#mock = mock;
    mock.on(
      { predicate: isTitleGenerationRequest },
      { content: "Scenario title" },
    );
    let nextTurn = 0;
    for (const [index, turn] of this.scenario.turns.entries()) {
      mock.on(
        {
          predicate: (request) =>
            !isTitleGenerationRequest(request) && nextTurn === index,
        },
        async (request: unknown) => {
          const { exchange, response } = await observeThenRespond(
            turn,
            request,
            index,
            this.hooks,
          );
          appendFileSync(this.turnLogPath, `${turn.label}\n`);
          this.exchanges.push(exchange);
          nextTurn = index + 1;
          return response;
        },
        turn.delay_ms ? { streamingProfile: { ttft: turn.delay_ms, tps: 1_000 } } : undefined,
      );
    }
    mock.on(
      {
        predicate: (request) =>
          !isTitleGenerationRequest(request) && nextTurn >= this.scenario.turns.length,
      },
      { content: "Scenario complete" },
    );
    await mock.start();
  }

  async stop(): Promise<void> {
    const mock = this.#mock;
    this.#mock = undefined;
    if (mock) await mock.stop();
  }
}

function messageToolCallId(message: Record<string, unknown>): string | undefined {
  const value = message.tool_call_id ?? message.toolCallId;
  return typeof value === "string" ? value : undefined;
}

export function toolResultForCall(
  exchanges: readonly RecordedMockExchange[],
  callId: string,
): { text: string; event: Record<string, unknown> } | undefined {
  for (const exchange of exchanges) {
    const messages = asRecord(exchange.request)?.messages;
    if (!Array.isArray(messages)) continue;
    for (const messageValue of messages) {
      const message = asRecord(messageValue);
      if (message?.role !== "tool" || messageToolCallId(message) !== callId) continue;
      const content = message.content;
      if (typeof content === "string") return { text: content, event: message };
      if (Array.isArray(content)) {
        const text = content
          .map((part) => asRecord(part)?.text)
          .filter((value): value is string => typeof value === "string")
          .join("");
        return { text, event: message };
      }
    }
  }
  return undefined;
}

export function toolCallsInTurn(turn: ScriptedTurn): ToolCallPlan[] {
  return turn.response.kind === "tool_calls" ? turn.response.calls : [];
}
