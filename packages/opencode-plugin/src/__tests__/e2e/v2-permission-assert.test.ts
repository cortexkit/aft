import { expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { bootV2Host, toolResult, until } from "./v2-host.js";

const binary = process.env.AFT_V2_HOST_BINARY;

function liveStoreCanary() {
  const roots = [
    process.env.XDG_DATA_HOME ?? join(homedir(), ".local/share"),
    process.env.XDG_STATE_HOME ?? join(homedir(), ".local/state"),
  ];
  const files: Record<string, string> = {};
  for (const root of roots) {
    const dir = join(root, "opencode");
    if (!existsSync(dir)) {
      files[dir] = "absent";
      continue;
    }
    for (const name of readdirSync(dir).filter((name) => /\.db(?:-wal|-shm)?$/.test(name))) {
      const path = join(dir, name);
      files[path] = createHash("sha256").update(readFileSync(path)).digest("hex");
    }
  }
  return files;
}

test.skipIf(!binary)(
  "real 2.1.0 assert allows, denies, returns feedback, aborts and saves always",
  async () => {
    const before = liveStoreCanary();
    const commands = {
      once: "echo assert-once-probe",
      deny: "echo assert-denied-probe",
      feedback: "echo assert-feedback-probe",
      reject: "echo assert-reject-probe",
      always: "echo assert-always-probe",
    };
    const host = await bootV2Host(binary as string, {
      label: "permission-assert",
      removeBuiltins: true,
      permissions: [
        { action: "shell", resource: "echo assert-*", effect: "ask" },
        { action: "shell", resource: commands.deny, effect: "deny" },
      ],
      script: Object.fromEntries(
        Object.entries(commands).map(([name, command]) => [
          name,
          {
            name: "shell",
            arguments: { command, description: `Run the ${name} permission probe` },
          },
        ]),
      ),
    });
    try {
      expect(host.version).toBe("2.1.0");
      const session = await host.newSession("Assert probes");
      async function reply(
        name: "once" | "feedback" | "reject" | "always",
        decision: "once" | "always" | "reject",
        message?: string,
      ) {
        const pending = host.turn(session.id, name);
        let requests = await host.client.permission.list({ sessionID: session.id });
        await until(`assert ${name} prompt`, async () => {
          requests = await host.client.permission.list({ sessionID: session.id });
          return requests.some((request) => request.resources.includes(commands[name]));
        });
        const request = requests.find((request) => request.resources.includes(commands[name]));
        expect(request?.action).toBe("shell");
        expect(request?.source?.type).toBe("tool");
        expect(request?.save?.length).toBeGreaterThan(0);
        console.log(`[assert-host] ${name} request=${JSON.stringify(request)}`);
        await host.client.permission.reply({
          sessionID: session.id,
          requestID: request!.id,
          decision,
          ...(message ? { message } : {}),
        });
        await pending;
        return toolResult(host.requests, commands[name]);
      }
      expect(await reply("once", "once")).toContain("assert-once-probe\n");
      expect(
        await host.client.permission.saved.list({ projectID: session.projectID }),
      ).toHaveLength(0);
      await host.turn(session.id, "deny");
      expect(toolResult(host.requests, commands.deny)).toContain("permission rules deny");
      expect(await host.client.permission.list({ sessionID: session.id })).toHaveLength(0);
      expect(await reply("feedback", "reject", "Use a narrower command")).toContain(
        "Use a narrower command",
      );
      await reply("reject", "reject");
      expect(toolResult(host.requests, commands.reject)).toBeUndefined();
      expect(await reply("always", "always")).toContain("assert-always-probe\n");
      const saved = await host.client.permission.saved.list({ projectID: session.projectID });
      console.log(`[assert-host] saved response=${JSON.stringify(saved)}`);
      expect(saved.some((rule) => rule.action === "shell")).toBe(true);
      const second = await host.newSession("Saved assert grant");
      await host.turn(second.id, "always");
      expect(await host.client.permission.list({ sessionID: second.id })).toHaveLength(0);
      console.log(`[assert-host] saved=${JSON.stringify(saved)}; all five outcomes observed`);
    } catch (error) {
      throw new Error(`${String(error)}\n${host.logs().slice(-18000)}`, { cause: error });
    } finally {
      await host.stop();
      expect(liveStoreCanary()).toEqual(before);
      console.log(`[assert-host] live-store canary unchanged: ${JSON.stringify(before)}`);
    }
  },
  180_000,
);
