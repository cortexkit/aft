import { describe, expect, test } from "bun:test";
import type { ChildProcess } from "node:child_process";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import { createRpcClient } from "../../helpers/rpc-client";

function fixture() {
  const stdin = new PassThrough();
  const stdout = new PassThrough();
  const writes: string[] = [];
  stdin.on("data", (chunk) => writes.push(String(chunk)));
  const child = Object.assign(new EventEmitter(), {
    stdin,
    stdout,
    killed: false,
    kill() {
      child.killed = true;
      queueMicrotask(() => child.emit("exit", 0, null));
      return true;
    },
  });
  const client = createRpcClient(child as unknown as ChildProcess);
  const frame = (message: Record<string, unknown>) => stdout.write(`${JSON.stringify(message)}\n`);
  return { child, client, frame, writes };
}

describe("Pi RPC client lifecycle", () => {
  test("late UI notifications after client close are not dispatched", async () => {
    const { client, frame, writes } = fixture();
    const delivered: unknown[] = [];
    client.onExtensionUIRequest((request) => {
      delivered.push(request);
      client.sendExtensionUIResponse({ id: request.id as string, confirmed: true });
    });
    const closing = client.close();
    // Pi can flush a notification after the harness has ended its stdin.
    expect(() =>
      frame({ type: "extension_ui_request", id: "late-notice", method: "notify", message: "LSP" }),
    ).not.toThrow();
    await closing;
    expect(delivered).toEqual([]);
    expect(writes).toEqual([]);
  });

  test("live confirm requests still receive their response", async () => {
    const { client, frame, writes } = fixture();
    client.onExtensionUIRequest((request) => {
      client.sendExtensionUIResponse({ id: request.id as string, confirmed: true });
    });
    frame({ type: "extension_ui_request", id: "confirm-1", method: "confirm" });
    expect(writes.map((line) => JSON.parse(line))).toEqual([
      { type: "extension_ui_response", id: "confirm-1", confirmed: true },
    ]);
    await client.close();
  });

  test("unexpected Pi exit still rejects pending commands", async () => {
    const { child, client } = fixture();
    const response = client.sendCommand({ type: "prompt", message: "hello" });
    child.emit("exit", 23, null);
    await expect(response).rejects.toThrow(
      "Pi RPC exited before completing pending work (code=23, signal=null)",
    );
  });
});
