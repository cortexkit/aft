import { expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { access, readFile } from "node:fs/promises";
import { join } from "node:path";

import {
  bootV2Host,
  type ModelScript,
  taskStatusResult,
  toolResult,
  until,
  type V2Host,
} from "./v2-host.js";

const hostBinary = process.env.AFT_V2_HOST_BINARY;

function alive(pid: number): boolean {
  try {
    process.kill(pid, 0);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ESRCH") return false;
    throw error;
  }
  try {
    const state = execFileSync("ps", ["-o", "stat=", "-p", String(pid)], {
      encoding: "utf8",
    }).trim();
    return state !== "" && !state.startsWith("Z");
  } catch (error) {
    if (
      (error as { status?: number; stdout?: string }).status === 1 &&
      String((error as { stdout?: string }).stdout ?? "").trim() === ""
    )
      return false;
    throw error;
  }
}

function groupAlive(pgid: number): boolean {
  return execFileSync("ps", ["-eo", "pgid=,stat="], { encoding: "utf8" })
    .split("\n")
    .some((line) => {
      const [group, state] = line.trim().split(/\s+/);
      return Number(group) === pgid && state !== undefined && !state.startsWith("Z");
    });
}

async function childSession(host: V2Host, script: ModelScript, parentID: string, label: string) {
  script[label] = {
    name: "subagent",
    arguments: { agent: "general", description: label, prompt: "Ready", background: true },
  };
  await host.turn(parentID, label);
  const children = await host.client.session.list();
  const child = children.data.find(
    (session) => session.parentID === parentID && session.title === label,
  );
  expect(child).toBeDefined();
  await host.client.session.wait({ sessionID: child!.id });
  return child!.id;
}

async function startGroup(host: V2Host, script: ModelScript, sessionID: string, label: string) {
  const marker = join(host.project, `${label}.pids`);
  const command = `sleep 300 & child=$!; printf '%s %s\\n' "$$" "$child" > '${marker}'; wait "$child"`;
  script[label] = {
    name: "shell",
    arguments: { command, description: "Start a background kill probe", background: true },
  };
  await host.turn(sessionID, label);
  await until(`${label} process group`, async () =>
    access(marker).then(
      () => true,
      () => false,
    ),
  );
  const [pid, child] = (await readFile(marker, "utf8")).trim().split(/\s+/).map(Number);
  const pgid = Number(
    execFileSync("ps", ["-o", "pgid=", "-p", String(pid)], { encoding: "utf8" }).trim(),
  );
  const taskId = toolResult(host.requests, command)?.match(/bash-[a-f0-9]+/)?.[0];
  expect(taskId).toBeDefined();
  expect(alive(pid)).toBe(true);
  expect(alive(child)).toBe(true);
  return { sessionID, taskId: taskId!, pid, child, pgid };
}

async function status(host: V2Host, script: ModelScript, sessionID: string, taskId: string) {
  const key = `status-${taskId}`;
  script[key] = { name: "bash_status", arguments: { taskId } };
  await host.turn(sessionID, key);
  const result = taskStatusResult(host.requests, taskId);
  if (result !== undefined) return result;
  throw new Error(`missing bash_status result for ${taskId}`);
}

for (const ending of ["stop", "reload-stop", "provider"] as const) {
  test.skipIf(!hostBinary)(
    `real V2 host: ${ending} ending kills only deliberately stopped subagent groups`,
    async () => {
      const script: ModelScript = {};
      const host = await bootV2Host(hostBinary as string, {
        label: `kill-${ending}`,
        script,
        removeBuiltins: true,
        userAftConfig: { lsp: { auto_install: false } },
        modelErrors: {
          fail: {
            status: 400,
            code: "invalid_request_error",
            message: "Injected provider failure",
          },
        },
      });
      const tasks: Array<Awaited<ReturnType<typeof startGroup>>> = [];
      try {
        console.log(`[v2-subagent-kill] OpenCode ${host.version}; ${ending}`);
        const parent = await host.newSession("Kill ownership");
        const stopped = await childSession(host, script, parent.id, "Stopped child");
        const sibling = await childSession(host, script, parent.id, "Sibling child");
        const parentTask = await startGroup(host, script, parent.id, "parent-task");
        tasks.push(parentTask);
        const siblingTask = await startGroup(host, script, sibling, "sibling-task");
        tasks.push(siblingTask);
        const owned = await startGroup(host, script, stopped, "owned-task");
        tasks.push(owned);
        expect(new Set(tasks.map((task) => task.pgid)).size).toBe(3);
        if (ending === "reload-stop") {
          const starts = host.logs().match(/AFT V2 runtime starting/g)?.length ?? 0;
          await host.client.location.reload();
          await host.client.location.get({ location: { directory: host.project } });
          await until(
            "fresh V2 runtime after reload",
            async () => (host.logs().match(/AFT V2 runtime starting/g)?.length ?? 0) > starts,
          );
          for (const task of tasks) expect(alive(task.pid) && alive(task.child)).toBe(true);
        }
        if (ending !== "provider") {
          const marker = join(host.project, "foreground.pids");
          script.hold = {
            name: "shell",
            arguments: {
              command: `echo $$ > '${marker}'; sleep 300`,
              description: "Start a waiting kill probe",
              wait: true,
            },
          };
          await host.client.session.prompt({ sessionID: stopped, text: "hold" });
          await until("active subagent execution", async () =>
            access(marker).then(
              () => true,
              () => false,
            ),
          );
          await host.client.session.interrupt({ sessionID: stopped });
          await host.client.session.wait({ sessionID: stopped });
          await until(
            "stopped background process group to disappear",
            async () => !alive(owned.pid) && !alive(owned.child) && !groupAlive(owned.pgid),
            8000,
          );
          await Bun.sleep(500);
          const context = await host.client.session.context({ sessionID: stopped });
          expect(
            context.some(
              (message) =>
                message.type === "synthetic" && String(message.text).includes(owned.taskId),
            ),
          ).toBe(false);
          expect(await status(host, script, stopped, owned.taskId)).toContain("killed");
          expect(alive(owned.pid)).toBe(false);
          expect(alive(owned.child)).toBe(false);
          expect(groupAlive(owned.pgid)).toBe(false);
        } else {
          await host.turn(stopped, "fail");
          expect((await host.client.session.get({ sessionID: stopped })).outcome).toBe("failed");
          await Bun.sleep(500);
          expect(alive(owned.pid)).toBe(true);
          expect(alive(owned.child)).toBe(true);
          expect(await status(host, script, stopped, owned.taskId)).toContain("running");
        }
        expect(alive(parentTask.pid)).toBe(true);
        expect(alive(parentTask.child)).toBe(true);
        expect(groupAlive(parentTask.pgid)).toBe(true);
        expect(await status(host, script, parent.id, parentTask.taskId)).toContain("running");
        expect(alive(siblingTask.pid)).toBe(true);
        expect(alive(siblingTask.child)).toBe(true);
        expect(groupAlive(siblingTask.pgid)).toBe(true);
        expect(await status(host, script, sibling, siblingTask.taskId)).toContain("running");
      } catch (error) {
        throw new Error(`${String(error)}\nV2 host output:\n${host.logs().slice(-16000)}`, {
          cause: error,
        });
      } finally {
        for (const task of tasks) {
          script.cleanup = { name: "bash_kill", arguments: { taskId: task.taskId } };
          await host.turn(task.sessionID, "cleanup").catch(() => {});
        }
        await host.stop();
      }
    },
    120000,
  );
}
