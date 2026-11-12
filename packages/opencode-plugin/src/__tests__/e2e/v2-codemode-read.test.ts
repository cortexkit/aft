import { expect, test } from "bun:test";
import { bootV2Host, messageText } from "./v2-host.js";

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

test.skipIf(!hostBinary)(
  "dotted Code Mode namespace preserves direct tools and nests under tools.aft.code",
  async () => {
    const host = await bootV2Host(hostBinary as string, {
      label: "codemode-namespace",
      defaultAftStorage: true,
      files: { "probe.txt": "dotted-namespace-read-content" },
      script: {
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
      await host.turn(session.id, "probe");
      const offered = host.requests.find((request) => request.tools?.length);
      const names = offered?.tools?.map((entry) => entry.function.name) ?? [];
      for (const name of [...directNames, "execute"]) expect(names).toContain(name);
      const outputs = host.requests
        .flatMap(({ messages }) =>
          messages
            .filter((message) => message.role === "tool")
            .map((message) => messageText(message.content)),
        )
        .join("\n");
      expect(outputs).toContain("dotted-namespace-read-content");
      for (const name of ["read", "grep", "glob", "search", "outline", "zoom", "callgraph"]) {
        expect(outputs).toContain(`"${name}"`);
      }
      console.log(
        `[codemode-namespace] OpenCode ${host.version}; direct=${names.join(",")}; result=${outputs}`,
      );
    } catch (error) {
      throw new Error(`${String(error)}\n${host.logs().slice(-18000)}`, { cause: error });
    } finally {
      await host.stop();
    }
  },
  180_000,
);
