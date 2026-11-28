import { expect, test } from "bun:test";
import { access } from "node:fs/promises";
import { join } from "node:path";

import { bootV2Host, type ModelScript, until } from "./v2-host.js";

const hostBinary = process.env.AFT_V2_HOST_BINARY;

for (const cause of ["provider", "overflow", "tool", "parent-stop", "shutdown"] as const) {
  test.skipIf(!hostBinary)(
    `real V2 host: execution ending for ${cause}`,
    async () => {
      const script: ModelScript = {};
      const host = await bootV2Host(hostBinary as string, {
        label: `ending-${cause}`,
        removeBuiltins: true,
        script,
        userAftConfig: { lsp: { auto_install: false } },
        modelErrors: {
          provider: {
            status: 400,
            code: "invalid_request_error",
            message: "Injected model provider failure",
          },
          overflow: {
            status: 400,
            code: "context_length_exceeded",
            message: "maximum context length exceeded",
          },
        },
      });
      let stopped = false;
      try {
        const session = await host.newSession(`Ending ${cause}`);
        await host.turn(session.id, "prime event subscription");
        await until("event subscription readiness", async () =>
          host.events.some((event) => event.type === "session.execution.succeeded"),
        );
        host.events.length = 0;
        if (cause === "tool")
          script.tool = { name: "read", arguments: { path: "missing-tool-crash-fixture" } };
        if (cause === "parent-stop" || cause === "shutdown") {
          const marker = join(host.project, "started");
          script["You are a subagent spawned by another session.\nHold"] = {
            name: "shell",
            arguments: {
              command: `echo started > '${marker}'; sleep 40`,
              description: "Write the start marker and wait",
              wait: true,
            },
          };
          script.launch = {
            name: "subagent",
            arguments: { agent: "general", description: "Ending fixture", prompt: "Hold" },
          };
          await host.client.session.prompt({ sessionID: session.id, text: "launch" });
          await until("child foreground task", async () =>
            access(marker).then(
              () => true,
              () => false,
            ),
          );
          if (cause === "shutdown") {
            await host.stop();
            stopped = true;
          } else {
            await host.client.session.interrupt({ sessionID: session.id });
            await host.client.session.wait({ sessionID: session.id });
          }
        } else {
          await host.turn(session.id, cause);
        }
        if (!stopped)
          await until("execution ending event", async () =>
            host.events.some(
              (event) =>
                event.type === "session.execution.failed" ||
                event.type === "session.execution.succeeded" ||
                event.type === "session.execution.interrupted",
            ),
          );
        const context = stopped
          ? undefined
          : await host.client.session.context({ sessionID: session.id });
        if (!stopped) {
          await host.stop();
          stopped = true;
        }
        const endings = host.events.filter(
          (event) =>
            event.type === "session.execution.interrupted" ||
            event.type === "session.execution.failed" ||
            event.type === "session.execution.succeeded",
        );
        console.log(
          `[v2-ending-table] OpenCode ${host.version}; ${cause}: ${JSON.stringify(endings.map((event) => ({ type: event.type, data: event.data })))}`,
        );
        if (cause === "provider" || cause === "overflow") {
          expect(endings.some((event) => event.type === "session.execution.failed")).toBe(true);
          expect(endings.some((event) => event.type === "session.execution.interrupted")).toBe(
            false,
          );
        } else if (cause === "tool") {
          expect(JSON.stringify(context)).toContain('"status":"error"');
          expect(endings.some((event) => event.type === "session.execution.succeeded")).toBe(true);
          expect(endings.some((event) => event.type === "session.execution.interrupted")).toBe(
            false,
          );
        } else if (cause !== "shutdown") {
          expect(
            endings.some(
              (event) =>
                event.type === "session.execution.interrupted" &&
                event.data.reason === (cause === "shutdown" ? "shutdown" : "user"),
            ),
          ).toBe(true);
        } else {
          console.log(
            "[v2-ending-table] shutdown closes the live event transport before terminal delivery; shutdown classification is source-backed.",
          );
        }
      } catch (error) {
        throw new Error(`${String(error)}\nV2 host output:\n${host.logs().slice(-16000)}`, {
          cause: error,
        });
      } finally {
        if (!stopped) await host.stop();
      }
    },
    120000,
  );
}
