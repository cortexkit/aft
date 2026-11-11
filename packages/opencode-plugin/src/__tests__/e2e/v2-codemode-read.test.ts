import { expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import { bootV2Host, type ModelScript, messageText, until } from "./v2-host.js";

const hostBinary = process.env.AFT_V2_HOST_BINARY;

test.skipIf(!hostBinary)(
  "Code Mode outside-root read prompts normally and resumes only after approval",
  async () => {
    const script: ModelScript = {};
    const host = await bootV2Host(hostBinary as string, {
      label: "codemode-read",
      script,
      permissions: [
        { action: "external_directory", resource: "*", effect: "ask" },
        { action: "read", resource: "*", effect: "ask" },
      ],
    });
    let pending: Promise<void> | undefined;
    try {
      expect(host.version).toBe("2.1.0");
      const outside = join(host.project, "..", "outside.txt");
      const contents = "outside-root-content-after-approval";
      await writeFile(outside, contents);
      script.read = {
        name: "execute",
        arguments: { code: `return await tools.read({ path: ${JSON.stringify(outside)} });` },
      };
      const outputs = () =>
        host.requests.flatMap(({ messages }) =>
          messages
            .filter((message) => message.role === "tool")
            .map((message) => messageText(message.content)),
        );
      const session = await host.newSession("Code Mode read approval");
      let finished = false;
      pending = host.turn(session.id, "read").finally(() => {
        finished = true;
      });
      void pending.catch(() => {});
      const actions = new Set(["external_directory", "read"]);
      while (actions.size > 0) {
        let requests = await host.client.permission.list({ sessionID: session.id });
        await until("Code Mode read permission prompt", async () => {
          requests = await host.client.permission.list({ sessionID: session.id });
          return requests.length > 0;
        });
        const request = requests[0]!;
        expect(actions.has(request.action)).toBe(true);
        expect(
          request.resources.some((resource) => resource.startsWith(join(host.project, ".."))),
        ).toBe(true);
        expect(request.source).toMatchObject({
          type: "tool",
          messageID: expect.any(String),
          id: expect.any(String),
        });
        expect(finished).toBe(false);
        expect(outputs().join("\n")).not.toContain(contents);
        console.log(`[codemode-read] prompt=${JSON.stringify(request)}`);
        await host.client.permission.reply({
          sessionID: session.id,
          requestID: request.id,
          decision: "once",
        });
        actions.delete(request.action);
      }
      await pending;
      expect(outputs().join("\n")).toContain(contents);
      expect(await host.client.permission.list({ sessionID: session.id })).toHaveLength(0);
      expect(existsSync(join(host.project, "..", "aft-storage", "aft.db"))).toBe(true);
      console.log(
        "[codemode-read] approved outside-root read returned content; isolated aft.db exists",
      );
    } catch (error) {
      throw new Error(`${String(error)}\n${host.logs().slice(-18000)}`, { cause: error });
    } finally {
      await host.stop();
      await pending?.catch(() => {});
    }
  },
  180_000,
);
