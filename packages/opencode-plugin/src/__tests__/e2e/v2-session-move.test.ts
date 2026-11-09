import { expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { copyFile, mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { bootV2Host, messageText, type ModelRequest, type ModelScript, until } from "./v2-host.js";

const hostBinary = process.env.AFT_V2_HOST_BINARY;

function latestDescriptionRead(requests: ModelRequest[]): string | undefined {
  for (const request of requests.toReversed()) {
    const reads = new Set(
      request.messages.flatMap((message) =>
        (message.tool_calls ?? [])
          .filter(
            (call) =>
              call.function.name === "read" &&
              JSON.parse(call.function.arguments).path === "DESCRIPTION",
          )
          .map((call) => call.id),
      ),
    );
    const result = request.messages
      .toReversed()
      .find((message) => message.role === "tool" && reads.has(message.tool_call_id ?? ""));
    if (result) return messageText(result.content);
  }
  return undefined;
}

test.skipIf(!hostBinary)(
  "real V2 host: relative reads follow a session moved to another repository",
  async () => {
    const script: ModelScript = {
      "read before move": { name: "read", arguments: { path: "DESCRIPTION" } },
      "read after move": { name: "read", arguments: { path: "DESCRIPTION" } },
    };
    const host = await bootV2Host(hostBinary as string, {
      label: "session-move",
      files: { DESCRIPTION: "first-repository-only\n" },
      script,
    });
    try {
      const destination = join(host.project, "..", "second-repository");
      await mkdir(join(destination, ".cortexkit"), { recursive: true });
      execFileSync("git", ["init", "-q"], { cwd: destination, stdio: "pipe" });
      await writeFile(join(destination, "DESCRIPTION"), "second-repository-only\n");
      await copyFile(
        join(host.project, ".cortexkit", "aft.jsonc"),
        join(destination, ".cortexkit", "aft.jsonc"),
      );
      const session = await host.newSession("Move between repositories");
      await host.turn(session.id, "read before move");
      expect(latestDescriptionRead(host.requests)).toContain("first-repository-only");
      expect(latestDescriptionRead(host.requests)).not.toContain("second-repository-only");

      const offered = host.requests.at(-1)?.tools ?? [];
      const move = offered.find((tool) => /^(opencode[._])?session_move$/.test(tool.function.name));
      if (move) {
        script["move the session"] = {
          name: move.function.name,
          arguments: { sessionID: session.id, directory: destination },
        };
        await host.turn(session.id, "move the session");
        console.log(`[v2-session-move] moved via ${move.function.name}`);
      } else {
        // The scripted model can call only directly offered tools. If session_move
        // is not directly offered, move through the host's real service API.
        await host.client.session.move({ sessionID: session.id, directory: destination });
        console.log(
          "[v2-session-move] session_move not directly offered; used client.session.move",
        );
      }
      await until("the host's session.moved event", async () =>
        host.events.some(
          (event) => event.type === "session.moved" && event.data.sessionID === session.id,
        ),
      );
      expect((await host.client.session.get({ sessionID: session.id })).location.directory).toBe(
        destination,
      );

      const afterMove = host.requests.length;
      await host.turn(session.id, "read after move");
      const result = latestDescriptionRead(host.requests.slice(afterMove));
      expect(result).toContain("second-repository-only");
      expect(result).not.toContain("first-repository-only");
      console.log(
        `[v2-session-move] OpenCode ${host.version}: relative DESCRIPTION followed the move`,
      );
    } catch (error) {
      throw new Error(`${String(error)}\nV2 host output:\n${host.logs().slice(-20_000)}`, {
        cause: error,
      });
    } finally {
      await host.stop();
    }
  },
  180_000,
);
