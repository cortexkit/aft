import { expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { copyFile, mkdir } from "node:fs/promises";
import { join } from "node:path";

import { withHermeticGitEnv } from "../../../../../tests/helpers/git-env.js";
import { bootV2Host, toolResult } from "./v2-host.js";

const hostBinary = process.env.AFT_V2_HOST_BINARY;

// A session whose Location is a linked git worktree runs its tools on the
// worktree's bridge. That bridge must receive the Location's configure
// overrides; without `bash_permissions` AFT runs shell commands without
// asking the host, so the host's deny rule below would never be consulted.
test.skipIf(!hostBinary)(
  "real V2 host: a linked-worktree session keeps the Location's shell permission override",
  async () => {
    const denied = "echo worktree-denied-probe";
    const host = await bootV2Host(hostBinary as string, {
      label: "linked-worktree",
      removeBuiltins: true,
      permissions: [{ action: "shell", resource: denied, effect: "deny" }],
      script: {
        deny: {
          name: "shell",
          arguments: { command: denied, description: "Run the denied worktree command" },
        },
      },
    });
    try {
      const git = (...args: string[]) =>
        execFileSync("git", args, { cwd: host.project, env: withHermeticGitEnv(), stdio: "pipe" });
      git(
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--allow-empty",
        "-m",
        "fixture",
      );
      const worktree = join(host.project, "..", "linked");
      git("worktree", "add", "-b", "linked", worktree);
      await mkdir(join(worktree, ".cortexkit"), { recursive: true });
      await copyFile(
        join(host.project, ".cortexkit", "aft.jsonc"),
        join(worktree, ".cortexkit", "aft.jsonc"),
      );
      await host.client.location.get({ location: { directory: worktree } });
      const session = await host.client.session.create({
        title: "Linked worktree",
        location: { directory: worktree },
      });
      await host.turn(session.id, "deny");
      const result = toolResult(host.requests, denied);
      expect(JSON.parse(result ?? "null")).toMatchObject({
        error: { type: "permission.rejected", message: expect.stringContaining(denied) },
      });
      expect(result).not.toContain("worktree-denied-probe\n");
    } catch (error) {
      throw new Error(`${String(error)}\n${host.logs().slice(-18000)}`, { cause: error });
    } finally {
      await host.stop();
    }
  },
  180_000,
);
