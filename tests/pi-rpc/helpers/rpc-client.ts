import type { ChildProcess } from "node:child_process";
import { attachJsonlReader } from "./jsonl";

export interface RpcClient {
  sendCommand<T = unknown>(
    command: Record<string, unknown>,
  ): Promise<{ success: boolean; error?: string; data?: T }>;
  sendExtensionUIResponse(response: {
    id: string;
    value?: string;
    confirmed?: boolean;
    cancelled?: boolean;
  }): void;
  onEvent(handler: (event: Record<string, unknown>) => void): void;
  onExtensionUIRequest(handler: (request: Record<string, unknown>) => void): void;
  waitForEvent(
    predicate: (event: Record<string, unknown>) => boolean,
    timeoutMs?: number,
  ): Promise<Record<string, unknown>>;
  close(): Promise<void>;
}

interface PendingCommand {
  resolve: (value: { success: boolean; error?: string; data?: unknown }) => void;
  reject: (reason: Error) => void;
}

export function createRpcClient(child: ChildProcess): RpcClient {
  if (!child.stdout) throw new Error("Pi RPC stdout was not piped");
  let nextId = 0;
  let closed = false;
  const pending = new Map<string, PendingCommand>();
  const events: Array<Record<string, unknown>> = [];
  const eventHandlers = new Set<(event: Record<string, unknown>) => void>();
  const uiHandlers = new Set<(request: Record<string, unknown>) => void>();
  const waiters = new Set<{
    predicate: (event: Record<string, unknown>) => boolean;
    resolve: (event: Record<string, unknown>) => void;
    reject: (error: Error) => void;
    timer: ReturnType<typeof setTimeout>;
  }>();

  const rejectAll = (error: Error): void => {
    for (const command of pending.values()) command.reject(error);
    pending.clear();
    for (const waiter of waiters) {
      clearTimeout(waiter.timer);
      waiter.reject(error);
    }
    waiters.clear();
  };

  const recordEvent = (event: Record<string, unknown>): void => {
    events.push(event);
    for (const handler of eventHandlers) handler(event);
    for (const waiter of [...waiters]) {
      if (!waiter.predicate(event)) continue;
      clearTimeout(waiter.timer);
      waiters.delete(waiter);
      waiter.resolve(event);
    }
  };

  attachJsonlReader(child.stdout, (line) => {
    if (line.trim().length === 0) return;
    let message: Record<string, unknown>;
    try {
      message = JSON.parse(line) as Record<string, unknown>;
    } catch (error) {
      recordEvent({ type: "invalid_json", line, error: String(error) });
      return;
    }

    if (message.type === "response" && typeof message.id === "string") {
      const command = pending.get(message.id);
      if (command) {
        pending.delete(message.id);
        command.resolve(message as { success: boolean; error?: string; data?: unknown });
      }
      return;
    }

    if (message.type === "extension_ui_request") {
      for (const handler of uiHandlers) handler(message);
      return;
    }

    recordEvent(message);
  });

  child.once("error", (error) => rejectAll(error));
  child.once("exit", (code, signal) => {
    closed = true;
    if (pending.size > 0 || waiters.size > 0) {
      rejectAll(
        new Error(`Pi RPC exited before completing pending work (code=${code}, signal=${signal})`),
      );
    }
  });

  const writeJsonLine = (payload: Record<string, unknown>): void => {
    if (closed || !child.stdin?.writable) throw new Error("Pi RPC stdin is closed");
    child.stdin.write(`${JSON.stringify(payload)}\n`);
  };

  return {
    sendCommand<T = unknown>(command: Record<string, unknown>) {
      const id = `req-${++nextId}`;
      const payload = { ...command, id };
      const promise = new Promise<{ success: boolean; error?: string; data?: T }>(
        (resolve, reject) => {
          pending.set(id, {
            resolve: (value) => resolve(value as { success: boolean; error?: string; data?: T }),
            reject,
          });
        },
      );
      writeJsonLine(payload);
      return promise;
    },
    sendExtensionUIResponse(response) {
      writeJsonLine({ type: "extension_ui_response", ...response });
    },
    onEvent(handler) {
      eventHandlers.add(handler);
    },
    onExtensionUIRequest(handler) {
      uiHandlers.add(handler);
    },
    waitForEvent(predicate, timeoutMs = 15_000) {
      const existing = events.find(predicate);
      if (existing) return Promise.resolve(existing);

      return new Promise<Record<string, unknown>>((resolve, reject) => {
        const timer = setTimeout(() => {
          waiters.delete(waiter);
          reject(new Error(`Timed out waiting ${timeoutMs}ms for Pi RPC event`));
        }, timeoutMs);
        const waiter = { predicate, resolve, reject, timer };
        waiters.add(waiter);
      });
    },
    async close() {
      if (closed) return;
      closed = true;
      child.stdin?.end();
      child.kill("SIGTERM");
      await new Promise<void>((resolve) => {
        const timer = setTimeout(() => {
          if (!child.killed) child.kill("SIGKILL");
          resolve();
        }, 2_000);
        child.once("exit", () => {
          clearTimeout(timer);
          resolve();
        });
      });
    },
  };
}
