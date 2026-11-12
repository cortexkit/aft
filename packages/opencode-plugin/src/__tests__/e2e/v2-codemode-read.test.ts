import { expect, test } from "bun:test";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import { bootV2Host, type ModelRequest, type ModelScript, messageText, until } from "./v2-host.js";

const hostBinary = process.env.AFT_V2_HOST_BINARY;
const directNames = [
  "read",
  "grep",
  "glob",
  "aft_search",
  "aft_outline",
  "aft_zoom",
  "aft_callgraph",
];

function outputs(requests: ModelRequest[]): string {
  return requests
    .flatMap(({ messages }) =>
      messages
        .filter((message) => message.role === "tool")
        .map((message) => messageText(message.content)),
    )
    .join("\n");
}

test.skipIf(!hostBinary)(
  "dotted Code Mode namespace preserves direct tools and nests under tools.aft.code",
  async () => {
    const host = await bootV2Host(hostBinary as string, {
      label: "codemode-namespace",
      defaultAftStorage: true,
      files: {
        "probe.txt": "dotted-namespace-read-content",
        "direct.txt": "direct-read-content",
      },
      script: {
        direct: { name: "read", arguments: { path: "direct.txt" } },
        probe: {
          name: "execute",
          arguments: {
            code: 'return { names: Object.keys(tools.aft.code), read: await tools.aft.code.read({ path: "probe.txt" }) };',
          },
        },
      },
    });
    try {
      expect(host.version).toBe("2.1.0");
      const session = await host.newSession("Dotted namespace probe");
      await host.turn(session.id, "direct");
      expect(outputs(host.requests)).toContain("direct-read-content");
      await host.turn(session.id, "probe");
      const offered = host.requests.find((request) => request.tools?.length);
      const names = offered?.tools?.map((entry) => entry.function.name) ?? [];
      for (const name of [...directNames, "execute"]) expect(names).toContain(name);
      const result = outputs(host.requests);
      expect(result).toContain("dotted-namespace-read-content");
      for (const name of ["read", "grep", "glob", "search", "outline", "zoom", "callgraph"]) {
        expect(result).toContain(`"${name}"`);
      }
      console.log(
        `[codemode-namespace] OpenCode ${host.version}; direct=${names.join(",")}; result=${result}`,
      );
    } catch (error) {
      throw new Error(`${String(error)}\n${host.logs().slice(-18000)}`, { cause: error });
    } finally {
      await host.stop();
    }
  },
  180_000,
);

test.skipIf(!hostBinary)(
  "Code Mode outside-root read prompts normally and resumes only after approval",
  async () => {
    const script: ModelScript = {};
    const host = await bootV2Host(hostBinary as string, {
      label: "codemode-read",
      defaultAftStorage: true,
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
        arguments: {
          code: `return await tools.aft.code.read({ path: ${JSON.stringify(outside)} });`,
        },
      };
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
        expect(outputs(host.requests)).not.toContain(contents);
        console.log(`[codemode-read] prompt=${JSON.stringify(request)}`);
        await host.client.permission.reply({
          sessionID: session.id,
          requestID: request.id,
          decision: "once",
        });
        actions.delete(request.action);
      }
      await pending;
      expect(outputs(host.requests)).toContain(contents);
      expect(await host.client.permission.list({ sessionID: session.id })).toHaveLength(0);
    } catch (error) {
      throw new Error(`${String(error)}\n${host.logs().slice(-18000)}`, { cause: error });
    } finally {
      await host.stop();
      await pending?.catch(() => {});
    }
  },
  180_000,
);

test.skipIf(!hostBinary)(
  "denying execute still offers the direct read-only tools and runs direct read",
  async () => {
    const host = await bootV2Host(hostBinary as string, {
      label: "codemode-denied",
      defaultAftStorage: true,
      files: { "direct.txt": "direct-read-with-execute-denied" },
      permissions: [{ action: "execute", resource: "*", effect: "deny" }],
      script: { direct: { name: "read", arguments: { path: "direct.txt" } } },
    });
    try {
      expect(host.version).toBe("2.1.0");
      const session = await host.newSession("Execute denied");
      await host.turn(session.id, "direct");
      const names =
        host.requests
          .find((request) => request.tools?.length)
          ?.tools?.map((entry) => entry.function.name) ?? [];
      for (const name of directNames) expect(names).toContain(name);
      expect(names).not.toContain("execute");
      expect(outputs(host.requests)).toContain("direct-read-with-execute-denied");
    } catch (error) {
      throw new Error(`${String(error)}\n${host.logs().slice(-18000)}`, { cause: error });
    } finally {
      await host.stop();
    }
  },
  180_000,
);
