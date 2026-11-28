import { expect, test } from "bun:test";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";

import { AftRpc } from "../../tui/aft-rpc";
import { bootV2Host, until } from "./v2-host.js";

const hostBinary = process.env.AFT_V2_HOST_BINARY;

test.skipIf(!hostBinary || process.platform === "win32")(
  "real V2 host lists, refreshes, and stops AFT tasks in the selected session",
  async () => {
    const waiting = "sleep 90";
    const finishing = "while [ ! -f finish-task ]; do sleep 0.1; done";
    const host = await bootV2Host(hostBinary as string, {
      label: "background-tasks",
      removeBuiltins: true,
      userAftConfig: { lsp: { auto_install: false } },
      script: {
        "start waiting": {
          name: "shell",
          arguments: {
            command: waiting,
            description: "Start long background task",
            background: true,
            timeout: 45_000,
          },
        },
        "start finishing": {
          name: "shell",
          arguments: {
            command: finishing,
            description: "Start short background task",
            background: true,
            timeout: 45_000,
          },
        },
      },
    });
    try {
      const rpc = host.client.rpc(AftRpc);
      const location = { location: { directory: host.project } };
      const { id: sessionID } = await host.newSession("Background tasks");
      const list = () => rpc.getBackgroundTasks({ sessionID }, location);
      const stop = (taskID: string) => rpc.stopBackgroundTask({ sessionID, taskID }, location);

      await host.turn(sessionID, "start waiting");
      const [first] = await list();
      expect(first).toMatchObject({ command: waiting, workdir: host.project });

      await host.turn(sessionID, "start finishing");
      const refreshed = await list();
      expect(refreshed.map((task) => task.command).sort()).toEqual([waiting, finishing].sort());
      const second = refreshed.find((task) => task.command === finishing)!;

      const { id: otherID } = await host.newSession("Other session");
      expect(await rpc.getBackgroundTasks({ sessionID: otherID }, location)).toEqual([]);
      expect(
        await rpc.stopBackgroundTask({ sessionID: otherID, taskID: first.id }, location),
      ).toEqual({ outcome: "not-active" });

      expect(await stop(first.id)).toEqual({ outcome: "stopped" });
      expect((await list()).map((task) => task.id)).toEqual([second.id]);

      await writeFile(join(host.project, "finish-task"), "");
      await until("the second task to finish on its own", async () => (await list()).length === 0);
      // The active-owner query no longer returns this task. It must not send a
      // kill or claim success after the task dropped out of the list.
      expect(await stop(second.id)).toEqual({ outcome: "not-active" });
    } finally {
      await host.stop();
    }
  },
  75_000,
);
