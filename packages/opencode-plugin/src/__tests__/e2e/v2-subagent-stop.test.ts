import { expect, test } from "bun:test";
import { access, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";

import { bootV2Host, type ModelScript, taskStatusResult, toolResult, until } from "./v2-host.js";

const hostBinary = process.env.AFT_V2_HOST_BINARY;

for (const scenario of [
  "sync",
  "sync-exit",
  "sync-gap",
  "plain",
  "async",
  "foreground",
  "steer",
  "main",
  "transient",
] as const) {
  test.skipIf(!hostBinary)(
    `real V2 host: ${scenario} watch interruption respects session role`,
    async () => {
      const script: ModelScript = {
        seed: {
          name: "subagent",
          arguments: {
            agent: "general",
            description: "Stop regression",
            prompt: "Ready",
            background: true,
          },
        },
      };
      const host = await bootV2Host(hostBinary as string, {
        label: `stop-${scenario}`,
        removeBuiltins: true,
        script,
        userAftConfig: { lsp: { auto_install: false } },
        modelErrors: {
          fail: {
            status: 400,
            code: "invalid_request_error",
            message: "Injected provider failure",
          },
        },
      });
      try {
        console.log(`[v2-subagent-stop] OpenCode ${host.version}; ${scenario}`);
        const parent = await host.newSession("Stop regression");
        let sessionID = parent.id;
        if (scenario !== "main") {
          await host.turn(parent.id, "seed");
          const children = await host.client.session.list();
          const child = children.data.find((session) => session.parentID === parent.id);
          expect(child).toBeDefined();
          sessionID = child!.id;
          await host.client.session.wait({ sessionID });
        }
        const marker = join(host.project, "release");
        const startedMarker = join(host.project, "started");
        const command = `echo $$ > '${startedMarker}'; while [ ! -f '${marker}' ]; do sleep 0.1; done; echo stop-test-done`;
        script.start = {
          name: "shell",
          arguments: {
            command,
            description: "Wait for the release marker",
            ...(scenario === "foreground" ? {} : { background: true }),
          },
        };
        await host.client.session.prompt({ sessionID, text: "start" });
        await until("command start", async () =>
          access(startedMarker).then(
            () => true,
            () => false,
          ),
        );
        let backgroundTaskId: string | undefined;
        if (scenario === "plain") await host.client.session.wait({ sessionID });
        if (scenario !== "foreground" && scenario !== "plain") {
          await host.client.session.wait({ sessionID });
          const result = toolResult(host.requests, command);
          const taskId = result?.match(/bash-[a-f0-9]+/)?.[0];
          backgroundTaskId = taskId;
          expect(taskId).toBeDefined();
          script.watch = {
            name: "bash_watch",
            arguments: {
              taskId,
              ...(scenario === "sync-exit" ? {} : { pattern: "stop-test-done" }),
              ...(scenario === "async" || scenario === "transient" ? { background: true } : {}),
            },
          };
          await host.client.session.prompt({ sessionID, text: "watch" });
          await until("watch tool executing", async () => {
            const context = await host.client.session.context({ sessionID });
            return JSON.stringify(context).includes('"name":"bash_watch"');
          });
          if (scenario === "async" || scenario === "transient")
            await host.client.session.wait({ sessionID });
        }
        // The command cannot finish until the interruption has settled.
        if (scenario === "async" || scenario === "plain") {
          const holdingMarker = join(host.project, "holding");
          script.hold = {
            name: "shell",
            arguments: {
              command: `echo holding > '${holdingMarker}'; sleep 40`,
              description: "Hold execution for the stop test",
              wait: true,
            },
          };
          await host.client.session.prompt({ sessionID, text: "hold" });
          await until("an active execution after watch registration", async () =>
            access(holdingMarker).then(
              () => true,
              () => false,
            ),
          );
        }
        if (scenario === "transient") {
          await host.turn(sessionID, "fail");
          expect((await host.client.session.get({ sessionID })).outcome).toBe("failed");
        } else if (scenario === "steer") {
          await host.client.session.prompt({ sessionID, text: "Continue" });
        } else if (scenario === "sync-gap") {
          await Promise.all([
            host.client.session.interrupt({ sessionID }),
            writeFile(marker, "release"),
          ]);
        } else {
          await host.client.session.interrupt({ sessionID });
        }
        await host.client.session.wait({ sessionID });
        if (scenario === "steer") {
          expect(JSON.stringify(host.requests.map((request) => request.messages))).toContain(
            "converted to an async watch",
          );
        }
        if (["foreground", "sync", "sync-exit", "plain", "async"].includes(scenario)) {
          const pid = Number(await readFile(startedMarker, "utf8"));
          await until("the interrupted foreground process to exit", async () => {
            try {
              process.kill(pid, 0);
              return false;
            } catch (error) {
              if ((error as NodeJS.ErrnoException).code === "ESRCH") return true;
              throw error;
            }
          });
        }
        const requestsBefore = host.requests.length;
        await writeFile(marker, "release");
        if (scenario === "steer" || scenario === "main" || scenario === "transient") {
          await until("completion wake", async () => {
            const context = await host.client.session.context({ sessionID });
            return context.some(
              (message) =>
                message.type === "synthetic" && String(message.text).includes("stop-test-done"),
            );
          });
        } else {
          // Allow both bridge notification delivery and its debounce window to run.
          await Bun.sleep(3000);
          const context = await host.client.session.context({ sessionID });
          expect(
            context.filter(
              (message) =>
                message.type === "synthetic" && String(message.text).includes("stop-test-done"),
            ),
          ).toEqual([]);
          expect(host.requests.length).toBe(requestsBefore);
          if (scenario !== "foreground") {
            script.resume = {
              name: "shell",
              arguments: { command: "echo resumed", description: "Print the resume marker" },
            };
            await host.turn(sessionID, "resume");
            const resumed = toolResult(host.requests, "echo resumed");
            if (scenario === "sync-gap") expect(resumed).toMatch(/killed|stop-test-done/);
            else if (scenario === "async") {
              script.status = { name: "bash_status", arguments: { taskId: backgroundTaskId } };
              await host.turn(sessionID, "status");
              expect(taskStatusResult(host.requests, backgroundTaskId!)).toContain("killed");
            } else expect(resumed).toContain("killed");
          }
        }
      } catch (error) {
        throw new Error(
          `${String(error)}\nV2 host output:\n${host.logs().slice(-20000)}\nRequests:\n${JSON.stringify(host.requests.map((request) => request.messages)).slice(-12000)}`,
          { cause: error },
        );
      } finally {
        await host.stop();
      }
    },
    120000,
  );
}
