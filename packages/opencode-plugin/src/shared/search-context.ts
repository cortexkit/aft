import { capSearchContext } from "@cortexkit/aft-bridge";
import { Effect } from "effect";
import type { ToolRuntime } from "../tools/_shared.js";
import type { PluginContext } from "../types.js";

const MESSAGE_LIMIT = 50;
const READ_TIMEOUT_MS = 1500;

function record(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

async function bounded<T>(read: (signal: AbortSignal) => Promise<T>): Promise<T | undefined> {
  const controller = new AbortController();
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      Promise.resolve().then(() => read(controller.signal)),
      new Promise<undefined>((resolve) => {
        timer = setTimeout(() => {
          controller.abort();
          resolve(undefined);
        }, READ_TIMEOUT_MS);
      }),
    ]);
  } catch {
    return undefined;
  } finally {
    clearTimeout(timer);
  }
}

function assistantText(parts: unknown[], runtime: ToolRuntime): string[] | undefined {
  const callID = runtime.hostVersion === 2 ? runtime.id : runtime.callID;
  const matches = parts.flatMap((value, index) => {
    const part = record(value);
    if (part?.type !== "tool") return [];
    const name = runtime.hostVersion === 2 ? part.name : part.tool;
    if (name !== "aft_search") return [];
    const id = runtime.hostVersion === 2 ? part.id : part.callID;
    if (callID) return id === callID ? [index] : [];
    // OpenCode 1 has no call id. More than one active search does not identify this call.
    const status = record(part.state)?.status;
    return typeof id === "string" && (status === "running" || status === "pending") ? [index] : [];
  });
  if (matches.length !== 1) return undefined;
  return textParts(parts.slice(0, matches[0]), true);
}

function textParts(parts: unknown[], reasoning = false): string[] {
  return parts.flatMap((value) => {
    const part = record(value);
    return (part?.type === "text" || (reasoning && part?.type === "reasoning")) &&
      typeof part.text === "string" &&
      part.text.trim()
      ? [part.text]
      : [];
  });
}

export async function collectOpenCodeSearchContext(
  ctx: PluginContext,
  runtime: ToolRuntime,
): Promise<string | undefined> {
  if (ctx.config.search?.gather?.enabled !== true) return undefined;
  if (!runtime.sessionID || !runtime.messageID) return undefined;
  const { sessionID, messageID } = runtime;
  const readV2 = ctx.readSearchMessageV2;
  if (runtime.hostVersion === 2 && (!runtime.id || !readV2)) return undefined;

  try {
    let preceding: string[] | undefined;
    let user: string[] = [];
    if (runtime.hostVersion === 2 && readV2) {
      const message = record(
        await bounded((signal) =>
          Effect.runPromise(readV2({ sessionID, messageID }), {
            signal: runtime.effectAbort ? AbortSignal.any([signal, runtime.effectAbort]) : signal,
          }),
        ),
      );
      if (
        message?.type !== "assistant" ||
        message.id !== messageID ||
        !Array.isArray(message.content)
      )
        return undefined;
      preceding = assistantText(message.content, runtime);
      // OpenCode 2 has no bounded history reader or user message id on assistant messages.
      // Do not load all messages to add the user text.
    } else {
      const response = await bounded(() =>
        ctx.client.session.message({ path: { id: sessionID, messageID } }),
      );
      const message = response?.data;
      if (
        message?.info.role !== "assistant" ||
        message.info.id !== messageID ||
        !Array.isArray(message.parts)
      )
        return undefined;
      preceding = assistantText(message.parts, runtime);
      if (!preceding) return undefined;
      const history = await bounded(() =>
        ctx.client.session.messages({ path: { id: sessionID }, query: { limit: MESSAGE_LIMIT } }),
      );
      if (!Array.isArray(history?.data)) return undefined;
      const newestUser = [...history.data].reverse().find((entry) => entry.info.role === "user");
      if (newestUser) user = textParts(newestUser.parts);
    }
    if (!preceding || ctx.config.search?.gather?.enabled !== true) return undefined;
    const text = [...preceding, ...user].join("\n\n");
    return text ? capSearchContext(text) : undefined;
  } catch {
    return undefined;
  }
}
