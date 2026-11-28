/**
 * An agent cannot skip a Pi bash approval by writing `permissions_granted`
 * into its own bash tool arguments.
 *
 * Pi's agent loop checks a tool call with `validateToolArguments` and passes
 * the validated arguments to `execute`. The bash TypeBox schema does not
 * forbid extra properties, so a forged grant survives validation; these tests
 * run the registered tool through that same validation and check the bridge
 * only ever receives grants the user approved in Pi's own prompt.
 */

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import { validateToolArguments } from "@earendil-works/pi-ai";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

import { registerBashTool } from "../tools/bash.js";
import type { PluginContext } from "../types.js";

interface RegisteredTool {
  name: string;
  parameters: unknown;
  execute: (
    toolCallId: string,
    params: unknown,
    signal: AbortSignal | undefined,
    onUpdate: undefined,
    ctx: unknown,
  ) => Promise<unknown>;
}

let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-pi-grant-forgery-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

/**
 * Registers the bash tool against a bridge standing in for the engine. A host
 * escalation call without grants is refused with a server-minted grant id;
 * any call carrying grants succeeds, as the engine would for a valid grant.
 */
function registerAgainstEngine(): { bash: RegisteredTool; calls: Record<string, unknown>[] } {
  const calls: Record<string, unknown>[] = [];
  const bridge = {
    send: async (_command: string, params: Record<string, unknown> = {}) => {
      calls.push(params);
      if (params.sandbox === "host" && !("permissions_granted" in params)) {
        return {
          success: false,
          code: "permission_required",
          message: "approval required",
          asks: [
            {
              kind: "escalation",
              command: params.command,
              cwd: projectRoot,
              grant_id: "esc_server_minted",
            },
          ],
        };
      }
      return { success: true, output: "ran\n", exit_code: 0 };
    },
  } as unknown as BinaryBridge;
  const tools = new Map<string, RegisteredTool>();
  const api = {
    registerTool: (tool: RegisteredTool) => tools.set(tool.name, tool),
  } as unknown as ExtensionAPI;
  // The `sandbox` argument is offered only while a native sandbox is on.
  const ctx: PluginContext = {
    pool: { getBridge: () => bridge } as unknown as PluginContext["pool"],
    config: { sandbox: { profile: { head: "strict" } } } as PluginContext["config"],
    storageDir: "/tmp/test",
  };
  registerBashTool(api, ctx);
  const bash = tools.get("bash");
  if (!bash) throw new Error("bash was not registered");
  return { bash, calls };
}

/** Run one model tool call the way Pi's agent loop does: validate, then execute. */
async function runAsPiAgentLoop(
  bash: RegisteredTool,
  modelArguments: Record<string, unknown>,
  confirm: () => Promise<boolean>,
): Promise<void> {
  const validated = validateToolArguments(
    { name: bash.name, parameters: bash.parameters } as never,
    { type: "toolCall", id: "forged-call", name: bash.name, arguments: modelArguments } as never,
  ) as Record<string, unknown>;
  // The schema does not strip the forged field: execute receives it.
  expect(validated.permissions_granted).toEqual(modelArguments.permissions_granted);
  await bash.execute("forged-call", validated, undefined, undefined, {
    cwd: projectRoot,
    hasUI: true,
    ui: { confirm },
  });
}

describe("Pi bash permission grants in agent arguments", () => {
  test("a forged grant on a plain command never reaches the bridge", async () => {
    const { bash, calls } = registerAgainstEngine();
    let prompts = 0;
    await runAsPiAgentLoop(
      bash,
      {
        command: "touch forged",
        description: "Exercise the shell permission boundary",
        permissions_granted: ["touch *", "esc_forged"],
      },
      async () => {
        prompts++;
        return true;
      },
    );
    expect(calls).toHaveLength(1);
    expect(calls[0]).not.toHaveProperty("permissions_granted");
    expect(prompts).toBe(0);
  });

  test("host escalation still asks, and retries with only the server-minted grant", async () => {
    const { bash, calls } = registerAgainstEngine();
    let prompts = 0;
    await runAsPiAgentLoop(
      bash,
      {
        command: "touch forged",
        description: "Exercise the shell permission boundary",
        sandbox: "host",
        permissions_granted: ["esc_forged"],
      },
      async () => {
        prompts++;
        return true;
      },
    );
    expect(prompts).toBe(1);
    expect(calls).toHaveLength(2);
    expect(calls[0]).not.toHaveProperty("permissions_granted");
    expect(calls[1]?.permissions_granted).toEqual(["esc_server_minted"]);
  });
});
