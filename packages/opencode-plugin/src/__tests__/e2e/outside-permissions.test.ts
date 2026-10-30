import { describe, expect, test } from "bun:test";
import {
  access,
  mkdir,
  mkdtemp,
  readdir,
  readFile,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import { Effect } from "effect";
import { callToolCall } from "../../tools/_shared.js";
import { projectV2Tool } from "../../tools/definitions/v2.js";
import { hoistedV2ToolConsumers } from "../../tools/hoisted/v2.js";
import { hoistedTools } from "../../tools/hoisted.js";
import { inspectTools } from "../../tools/inspect.js";
import type { PluginContext } from "../../types.js";
import { permissionHost } from "../v2-permission-host.js";
import { configureParamsFromLegacyOverrides, createHarness, prepareBinary } from "./helpers.js";

const binary = await prepareBinary();

async function stdoutPath(storage: string, id: string): Promise<string> {
  const pending = [storage];
  while (pending.length) {
    const directory = pending.pop();
    if (!directory) break;
    // The engine's index builders create and remove temporary folders under
    // the same storage root while this walk runs, so a listed folder can be
    // gone by the time it is read; skip it.
    const entries = await readdir(directory, { withFileTypes: true }).catch(
      (error: NodeJS.ErrnoException) => {
        if (error.code === "ENOENT") return [];
        throw error;
      },
    );
    for (const entry of entries) {
      const path = join(directory, entry.name);
      if (entry.isDirectory()) pending.push(path);
      else if (entry.name === `${id}.stdout` || (entry.name === "stdout" && path.includes(id)))
        return path;
    }
  }
  throw new Error(`no stdout artifact for the real task ${id}`);
}

describe.skipIf(!binary.binaryPath)("real engine outside exemptions", () => {
  test("grep honors outside directory approval through a symlinked scratch parent", async () => {
    const h = await createHarness(binary, {
      fixtureNames: [],
      configOverrides: configureParamsFromLegacyOverrides({ restrict_to_project_root: false }),
    });
    const outside = await mkdtemp(join(homedir(), ".cache", "aft-outside-grep-"));
    try {
      const repository = join(outside, "repositories", "antigravity-auth-314-prs");
      const packages = join(repository, "packages");
      await mkdir(packages, { recursive: true });
      await writeFile(join(packages, "probe.ts"), "export const outside_grep_canary = 1;\n");
      const scratch = join(outside, "CortexKit-scratch");
      await mkdir(scratch);
      await symlink(join(outside, "repositories"), join(scratch, "repos"), "junction");
      const ctx = {
        pool: { getBridge: () => h.bridge } as unknown as BridgePool,
        config: {},
        client: { session: { get: async () => ({ data: { directory: h.tempDir } }) } },
        storageDir: h.path(".aft-storage"),
      } as unknown as PluginContext;
      for (const path of [
        packages,
        join(scratch, "repos", "antigravity-auth-314-prs", "packages"),
      ]) {
        let questions = 0;
        const runtime = {
          directory: h.tempDir,
          sessionID: "outside-grep-session",
          callID: path,
          ask: async () => {
            questions++;
          },
        };
        const result = await callToolCall(ctx, runtime, "grep", {
          path,
          pattern: "outside_grep_canary",
        });
        expect(result.success).toBe(true);
        expect(result.text).toContain("outside_grep_canary");
        expect(questions).toBe(1);
      }
    } finally {
      await h.cleanup();
      await rm(outside, { recursive: true, force: true });
    }
  }, 60_000);

  test("mixed delete waits for every outside approval before any unlink", async () => {
    const h = await createHarness(binary, {
      fixtureNames: [],
      configOverrides: configureParamsFromLegacyOverrides({ restrict_to_project_root: false }),
    });
    const outside = await mkdtemp(join(homedir(), ".cache", "aft-outside-delete-"));
    try {
      const files = [join(h.tempDir, "inside.txt"), join(outside, "a.txt"), join(outside, "b.txt")];
      await Promise.all(files.map((path) => writeFile(path, "must survive pending approvals")));
      let questions = 0;
      const ctx = {
        pool: { getBridge: () => h.bridge } as unknown as BridgePool,
        config: {},
        client: { session: { get: async () => ({ data: { directory: h.tempDir } }) } },
        storageDir: h.path(".aft-storage"),
      } as unknown as PluginContext;
      const runtime = {
        directory: h.tempDir,
        sessionID: "delete-approval-session",
        callID: "one-delete-call",
        ask: async () => {
          questions++;
          await Promise.all(files.map((path) => access(path)));
        },
      };
      const result = await callToolCall(ctx, runtime, "delete", { files });
      expect(result.success).toBe(true);
      expect(questions).toBe(2);
      for (const path of files) await expect(access(path)).rejects.toThrow();
    } finally {
      await h.cleanup();
      await rm(outside, { recursive: true, force: true });
    }
  }, 60_000);
  for (const version of [1, 2]) {
    test(`OpenCode ${version} temp and own-output reads need zero external decisions`, async () => {
      const scratch = await mkdtemp(
        join(process.platform === "win32" ? tmpdir() : "/tmp", "aft-exempt-"),
      );
      const cache = join(homedir(), ".cache");
      await mkdir(cache, { recursive: true });
      const environment = await mkdtemp(join(cache, "aft-exempt-env-"));
      const h = await createHarness(binary, {
        childEnvironmentRoot: environment,
        fixtureNames: [],
        configOverrides: configureParamsFromLegacyOverrides({
          restrict_to_project_root: false,
          bash_permissions: false,
        }),
        bridgeOptions: {
          timeoutMs: 20_000,
          childEnv: { TMPDIR: scratch, TEMP: scratch, TMP: scratch },
        },
      });
      const storage = join(environment, "storage");
      const session = `real-exempt-v${version}`;
      try {
        await mkdir(join(h.tempDir, ".git"));
        await h.bridge.send(
          "configure",
          configureParamsFromLegacyOverrides({
            project_root: h.tempDir,
            harness: "opencode",
            storage_dir: storage,
            restrict_to_project_root: false,
            bash_permissions: false,
          }),
        );
        const canary = join(environment, "outside-project.txt");
        await writeFile(canary, "ordinary outside access still needs approval");
        const question = await h.bridge.toolCall(session, "read", { path: canary });
        expect(question.code).toBe("permission_required");
        const ordinary = join(scratch, "ordinary.txt");
        await writeFile(ordinary, "ordinary engine temp contents\n");
        const task = await h.bridge.send("bash", {
          session_id: session,
          command: "echo own-engine-output",
          background: true,
        });
        expect(typeof task.task_id).toBe("string");
        await h.bridge.send("bash_watch", {
          session_id: session,
          task_id: task.task_id,
          pattern: "own-engine-output",
          timeout_ms: 10_000,
        });
        const artifact = await stdoutPath(storage, String(task.task_id));
        const ctx = {
          pool: { getBridge: () => h.bridge } as unknown as BridgePool,
          client: { session: { get: async () => ({ data: { directory: h.tempDir } }) } },
          config: {},
          storageDir: storage,
        } as unknown as PluginContext;
        const external: unknown[] = [];
        if (version === 1) {
          const runtime = {
            directory: h.tempDir,
            sessionID: session,
            messageID: "exempt-message",
            ask: async (input: { permission: string }) => {
              if (input.permission === "external_directory") external.push(input);
            },
          };
          for (const [path, contents] of [
            [ordinary, "ordinary engine temp contents"],
            [artifact, "own-engine-output"],
          ] as const) {
            expect((await callToolCall(ctx, runtime, "read", { path })).text).toContain(contents);
          }
        } else {
          const host = {
            permission: {
              assert: (input: { action: string }) =>
                Effect.sync(() => {
                  if (input.action === "external_directory") external.push(input);
                }),
            },
          };
          const definitions = hoistedTools(ctx);
          if (!definitions.read) throw new Error("read definition missing");
          const read = projectV2Tool(
            "read",
            definitions.read,
            { directory: h.tempDir },
            hoistedV2ToolConsumers(host),
          );
          const execution = {
            sessionID: session,
            messageID: "exempt-message",
            id: "exempt-call",
            agent: "build",
            progress: () => Effect.void,
          };
          for (const [path, contents] of [
            [ordinary, "ordinary engine temp contents"],
            [artifact, "own-engine-output"],
          ] as const) {
            const result = await Effect.runPromise(read.execute({ path }, execution));
            expect(JSON.stringify(result)).toContain(contents);
          }
        }
        expect(external).toEqual([]);
        // Another session is refused outright by the storage floor: no
        // contents, no approval offered, and the artifact is left unchanged.
        const before = await readFile(artifact, "utf8");
        const foreign = await h.bridge.toolCall("other-session", "read", { path: artifact });
        expect(foreign.success).toBe(false);
        expect(foreign.code).toBe("denied_floor");
        expect(foreign).not.toHaveProperty("asks");
        expect(JSON.stringify(foreign)).not.toContain("own-engine-output");
        const foreignWrite = await h.bridge.toolCall("other-session", "write", {
          path: artifact,
          content: "foreign overwrite",
        });
        expect(foreignWrite.success).toBe(false);
        expect(foreignWrite.code).toBe("denied_floor");
        expect(foreignWrite).not.toHaveProperty("asks");
        expect(await readFile(artifact, "utf8")).toBe(before);
      } finally {
        await h.cleanup();
        await rm(environment, { recursive: true, force: true });
        await rm(scratch, { recursive: true, force: true });
      }
    }, 60_000);
  }

  test("OpenCode 1 outside-file inspect asks, runs once approved, and a denial ends it", async () => {
    const h = await createHarness(binary, {
      fixtureNames: [],
      configOverrides: configureParamsFromLegacyOverrides({ restrict_to_project_root: false }),
    });
    const cache = join(homedir(), ".cache");
    await mkdir(cache, { recursive: true });
    const outside = await mkdtemp(join(cache, "aft-outside-inspect-"));
    try {
      const file = join(outside, "probe.ts");
      await writeFile(file, "// TODO: outside inspect\nexport const probe = 1;\n");
      const ctx = {
        pool: { getBridge: () => h.bridge } as unknown as BridgePool,
        config: {},
        client: { session: { get: async () => ({ data: { directory: h.tempDir } }) } },
        storageDir: h.path(".aft-storage"),
      } as unknown as PluginContext;
      const asked: Array<{ permission: string }> = [];
      const runtime = {
        directory: h.tempDir,
        sessionID: "inspect-outside-v1",
        callID: "inspect-allowed",
        ask: async (input: { permission: string }) => {
          asked.push(input);
        },
      };
      const args = { scope: file, sections: "todos" };
      const result = await callToolCall(ctx, runtime, "inspect", args);
      expect(result.success).not.toBe(false);
      expect(result.code).toBeUndefined();
      expect(asked.map((input) => input.permission)).toEqual(["external_directory"]);
      const denied = {
        ...runtime,
        callID: "inspect-denied",
        ask: async () => {
          throw new Error("user rejected outside inspect");
        },
      };
      await expect(callToolCall(ctx, denied, "inspect", args)).rejects.toThrow(/denied|rejected/i);
    } finally {
      await h.cleanup();
      await rm(outside, { recursive: true, force: true });
    }
  }, 60_000);

  test("OpenCode 2 outside-file inspect asks, runs once approved, and a denial ends it", async () => {
    const h = await createHarness(binary, {
      fixtureNames: [],
      configOverrides: configureParamsFromLegacyOverrides({ restrict_to_project_root: false }),
    });
    const cache = join(homedir(), ".cache");
    await mkdir(cache, { recursive: true });
    const outside = await mkdtemp(join(cache, "aft-outside-inspect-"));
    try {
      const file = join(outside, "probe.ts");
      await writeFile(file, "// TODO: outside inspect\nexport const probe = 1;\n");
      const ctx = {
        pool: { getBridge: () => h.bridge } as unknown as BridgePool,
        config: {},
        client: { session: { get: async () => ({ data: { directory: h.tempDir } }) } },
        storageDir: h.path(".aft-storage"),
      } as unknown as PluginContext;
      const definition = inspectTools(ctx).aft_inspect;
      if (!definition) throw new Error("aft_inspect definition missing");
      const run = (deny: boolean, id: string) => {
        const host = permissionHost({
          agentRules: [{ action: "*", resource: "*", effect: "allow" }],
          evaluate: (input, effect) =>
            deny && input.action === "external_directory" ? "deny" : effect,
        });
        const tool = projectV2Tool(
          "aft_inspect",
          definition,
          { directory: h.tempDir },
          hoistedV2ToolConsumers(host.host),
        );
        const execution = {
          sessionID: "inspect-outside-v2",
          messageID: "inspect-message",
          id,
          agent: "build",
          progress: () => Effect.void,
        };
        return {
          calls: host.calls,
          result: Effect.runPromise(tool.execute({ scope: file, sections: "todos" }, execution)),
        };
      };
      const allowed = run(false, "inspect-allowed");
      const result = await allowed.result;
      expect(JSON.stringify(result)).not.toContain("permission_required");
      expect(allowed.calls.map((call) => call.action)).toContain("external_directory");
      const denied = run(true, "inspect-denied");
      await expect(denied.result).rejects.toThrow("external_directory");
    } finally {
      await h.cleanup();
      await rm(outside, { recursive: true, force: true });
    }
  }, 60_000);
});
