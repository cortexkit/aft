/**
 * OpenCode 2 registers AFT's bash as `shell` and its apply_patch as `patch`.
 *
 * Runs a real OpenCode 2 host with AFT and a scripted model (see `v2-host.ts`)
 * and checks what the model is offered and what happens when it calls
 * `shell`:
 * - the model sees one `shell` and one `patch`, both AFT's, and no `bash` or
 *   `apply_patch`, with and without the user config entries that remove the
 *   host's own shell and patch plugins: those entries remove only the
 *   built-ins, never AFT's tools under the same names;
 * - without those entries the host's built-in shell plugin rewrites the
 *   `shell` description on every request, and AFT puts its own back;
 * - a `shell` call runs through AFT: AFT's engine rewrites `cat` into a read
 *   and its footer names `shell`, the tool the model can call;
 * - its permission is evaluated under `shell`: a deny rule for `shell`
 *   refuses the command;
 * - a new message detaches a waiting `shell({wait:true})` call.
 *
 * Needs AFT_V2_HOST_BINARY (the load matrix supplies a pinned host), a built
 * `dist/`, and an `aft` binary built from this checkout.
 */
import { expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { join } from "node:path";

import { aftBinary, bootV2Host, toolResult, until } from "./v2-host.js";

const hostBinary = process.env.AFT_V2_HOST_BINARY;
const REWRITE = "cat probe.txt";
const FORBIDDEN_MARKER = "forbidden-probe.txt";
const FORBIDDEN = `echo forbidden-probe > ${FORBIDDEN_MARKER}`;
const WAITING = "sleep 45; echo waited-out";

async function checkHost(label: string, removeBuiltins: boolean) {
  const host = await bootV2Host(hostBinary as string, {
    label,
    removeBuiltins,
    files: { "probe.txt": "rewrite-probe-line\n" },
    permissions: [{ action: "shell", resource: "echo forbidden*", effect: "deny" }],
    script: {
      "run the rewrite probe": {
        name: "shell",
        arguments: { command: REWRITE, description: "Read the rewrite probe file" },
      },
      "run the forbidden probe": {
        name: "shell",
        arguments: { command: FORBIDDEN, description: "Run the forbidden probe" },
      },
      "run the waiting probe": {
        name: "shell",
        arguments: { command: WAITING, description: "Wait for the probe command", wait: true },
      },
    },
  });
  try {
    console.log(`[v2-tool-names:${label}] OpenCode ${host.version}`);
    const session = await host.newSession("Tool names");

    // What the model is offered.
    await host.turn(session.id, "list your tools");
    const offered = host.requests.find((request) => request.tools?.length);
    if (!offered?.tools) throw new Error("no tool-bearing model request");
    const names = offered.tools.map((tool) => tool.function.name);
    console.log(`[v2-tool-names:${label}] tools=${[...names].sort().join(",")}`);
    expect(names.filter((name) => name === "shell")).toHaveLength(1);
    expect(names.filter((name) => name === "patch")).toHaveLength(1);
    expect(names).not.toContain("bash");
    expect(names).not.toContain("apply_patch");
    expect(names).toContain("bash_status");
    const describe = (name: string) =>
      offered.tools?.find((tool) => tool.function.name === name)?.function.description ?? "";
    // AFT's own descriptions, whether or not the host's built-in shell plugin
    // is loaded and rewriting the `shell` description on each request.
    expect(describe("shell")).toStartWith("Execute shell commands.");
    expect(describe("patch")).toStartWith("Use the `patch` tool to edit files.");

    // A shell call runs through AFT's engine: it rewrites the `cat` into a read
    // and appends a hint that names the command tool as `shell`.
    await host.turn(session.id, "run the rewrite probe");
    const rewritten = toolResult(host.requests, REWRITE);
    console.log(`[v2-tool-names:${label}] rewrite result=${JSON.stringify(rewritten)}`);
    expect(rewritten).toContain("rewrite-probe-line");
    expect(rewritten).toContain("Prefer `read` tool over shell.");

    // The permission is filed under `shell`, where the host's deny rule is.
    await host.turn(session.id, "run the forbidden probe");
    const refused = toolResult(host.requests, FORBIDDEN);
    console.log(`[v2-tool-names:${label}] forbidden result=${JSON.stringify(refused)}`);
    // Check the filesystem independently of the host's rendering: a refusal
    // must prevent the command from executing, not merely hide its stdout.
    expect(existsSync(join(host.project, FORBIDDEN_MARKER))).toBe(false);
    expect(refused).toBeDefined();
    expect(JSON.parse(refused as string).error.type).toBe("permission.rejected");

    // A new message detaches a waiting shell call long before its command ends.
    const waitSession = await host.newSession("Waiting shell");
    const started = Date.now();
    await host.client.session.prompt({ sessionID: waitSession.id, text: "run the waiting probe" });
    await until("the model to issue the waiting call", async () => host.issued.includes(WAITING));
    // Give the host a moment to hand the call to AFT before the new message.
    await Bun.sleep(2000);
    await host.client.session.prompt({ sessionID: waitSession.id, text: "stop waiting" });
    await host.client.session.wait({ sessionID: waitSession.id });
    const elapsed = Date.now() - started;
    const detached = toolResult(host.requests, WAITING);
    console.log(
      `[v2-tool-names:${label}] waiting result after ${elapsed}ms=${JSON.stringify(detached)}`,
    );
    expect(detached).toContain("Detached because a user message arrived.");
    expect(detached).not.toContain("waited-out\n");
    expect(elapsed).toBeLessThan(40_000);
  } catch (error) {
    throw new Error(`${String(error)}\nV2 host output:\n${host.logs().slice(-20_000)}`, {
      cause: error,
    });
  } finally {
    await host.stop();
  }
}

test.skipIf(!hostBinary)(
  "real V2 host offers AFT's shell and patch under the host's names and runs shell through AFT",
  async () => {
    expect(existsSync(aftBinary)).toBe(true);
    await checkHost("builtins-removed", true);
    await checkHost("builtins-loaded", false);
  },
  240_000,
);
