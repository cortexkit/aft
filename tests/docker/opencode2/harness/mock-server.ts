import { createRequire } from "node:module";
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

import type {
  RecordedMockExchange,
  ScenarioDefinition,
  ScriptedTurn,
  ToolCallPlan,
} from "./types.js";
import { asRecord } from "./util.js";

interface LLMockLike {
  url: string;
  on(
    matcher: { sequenceIndex?: number; predicate?: (request: unknown) => boolean },
    response: unknown,
    options?: unknown,
  ): void;
  onMessage(pattern: string, response: unknown): void;
  start(): Promise<void>;
  stop(): Promise<void>;
}

interface LLMockConstructor {
  new (options: { port: number }): LLMockLike;
}

export interface DeterministicMockHooks {
  beforeTurn?: (turn: ScriptedTurn, request: unknown) => void | Promise<void>;
  afterRequest?: (exchange: RecordedMockExchange) => void | Promise<void>;
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

  async start(port = 0): Promise<void> {
    mkdirSync(dirname(this.turnLogPath), { recursive: true });
    writeFileSync(this.turnLogPath, "");
    const require = createRequire(import.meta.url);
    const loaded = require("@copilotkit/aimock") as { LLMock?: LLMockConstructor };
    if (!loaded.LLMock) throw new Error("@copilotkit/aimock does not export LLMock");
    const mock = new loaded.LLMock({ port });
    this.#mock = mock;
    for (const [index, turn] of this.scenario.turns.entries()) {
      mock.on(
        { sequenceIndex: index },
        async (request: unknown) => {
          await this.hooks.beforeTurn?.(turn, request);
          const response = mockResponse(turn);
          appendFileSync(this.turnLogPath, `${turn.label}\n`);
          const exchange: RecordedMockExchange = {
            index,
            label: turn.label,
            request,
            response,
            observed_at: new Date().toISOString(),
          };
          this.exchanges.push(exchange);
          await this.hooks.afterRequest?.(exchange);
          return response;
        },
        turn.delay_ms ? { streamingProfile: { ttft: turn.delay_ms, tps: 1_000 } } : undefined,
      );
    }
    mock.onMessage(".*", async (request: unknown) => {
      const response = { content: "UNEXPECTED_TURN_FALLBACK" };
      appendFileSync(this.turnLogPath, "unexpected-fallback\n");
      const exchange: RecordedMockExchange = {
        index: this.exchanges.length,
        label: "unexpected-fallback",
        request,
        response,
        observed_at: new Date().toISOString(),
      };
      this.exchanges.push(exchange);
      await this.hooks.afterRequest?.(exchange);
      return response;
    });
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
