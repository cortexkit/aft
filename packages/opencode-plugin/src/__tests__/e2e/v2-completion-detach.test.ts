/**
 * A background completion detaches a waiting command call on a real OpenCode 2
 * host, as a new user message does.
 *
 * The host admits AFT's completion wake through `session.synthetic`, which
 * does not run the session `prompt` hook where a typed message detaches a
 * waiting call, so AFT detaches the session's waits itself once the wake is
 * admitted. Without that, a `wait: true` call started before the background
 * task finished sits out its whole command even though the result the agent is
 * waiting for has arrived.
 *
 * Needs AFT_V2_HOST_BINARY (the load matrix supplies a pinned host), a built
 * `dist/`, and an `aft` binary; see `v2-host.ts`.
 */
import { expect, test } from "bun:test";
import { existsSync } from "node:fs";

import { aftBinary, bootV2Host, toolResult, until } from "./v2-host.js";

const hostBinary = process.env.AFT_V2_HOST_BINARY;
/** The name AFT's command tool (its `bash`) is registered under on OpenCode 2. */
const COMMAND_TOOL = "shell";
// Long enough that the waiting call is running before the task finishes: a
// completion that arrives first has no wait to detach.
const BACKGROUND = "sleep 8; echo background-done";
const WAITING = "sleep 40; echo waited-out";

test.skipIf(!hostBinary)(
  "real V2 host: a background completion detaches a waiting command call",
  async () => {
    expect(existsSync(aftBinary)).toBe(true);
    const host = await bootV2Host(hostBinary as string, {
      label: "completion-detach",
      removeBuiltins: true,
      script: {
        "start the background task": {
          name: COMMAND_TOOL,
          arguments: {
            command: BACKGROUND,
            description: "Start background task",
            background: true,
          },
        },
        "wait for the long command": {
          name: COMMAND_TOOL,
          arguments: { command: WAITING, description: "Wait for long command", wait: true },
        },
      },
    });
    try {
      console.log(`[v2-completion-detach] OpenCode ${host.version}`);
      const session = await host.newSession("Completion detach");
      await host.turn(session.id, "start the background task");
      expect(toolResult(host.requests, BACKGROUND)).toContain("background");

      const started = Date.now();
      await host.client.session.prompt({
        sessionID: session.id,
        text: "wait for the long command",
      });
      await until("the model to issue the waiting call", async () => host.issued.includes(WAITING));
      await host.client.session.wait({ sessionID: session.id });
      const elapsed = Date.now() - started;
      const waited = toolResult(host.requests, WAITING);
      console.log(
        `[v2-completion-detach] waiting result after ${elapsed}ms=${JSON.stringify(waited)}`,
      );

      // The background task's completion reached the session as a wake...
      const context = await host.client.session.context({ sessionID: session.id });
      expect(
        context.some(
          (message) =>
            message.type === "synthetic" && String(message.text).includes("background-done"),
        ),
      ).toBe(true);
      // ...and the waiting call came back detached, long before its command ended.
      expect(waited).toBeDefined();
      expect(waited).toContain("Detached");
      expect(waited).not.toContain("waited-out\n");
      expect(elapsed).toBeLessThan(30_000);
    } catch (error) {
      throw new Error(`${String(error)}\nV2 host output:\n${host.logs().slice(-20_000)}`, {
        cause: error,
      });
    } finally {
      await host.stop();
    }
  },
  120_000,
);
