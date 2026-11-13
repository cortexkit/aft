/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { tool } from "@opencode-ai/plugin";
import { Effect } from "effect";

import type { AftConfig } from "../config.js";
import { buildAftToolDefinitions, registerAftTools } from "../tool-registration.js";
import type { V2ProviderTool } from "../tools/definitions/v2.js";
import type { PluginContext } from "../types.js";
import {
  aftToolNameForV2Host,
  renameV2ToolReferences,
  V2_HOST_TOOL_NAMES,
  v2HostToolName,
} from "../v2-host-tool-names.js";
import { registerV2ToolHooks } from "../v2-tool-hooks.js";
import { buildHintsForRegisteredTools } from "../workflow-hints.js";

const CONFIG: AftConfig = { disabled_tools: [] };

function definitions(config: AftConfig = CONFIG) {
  return buildAftToolDefinitions(
    {
      pool: {},
      client: {},
      config,
      hashlineEffective: false,
      storageDir: "/storage",
    } as unknown as PluginContext,
    config,
  );
}

function projected(config: AftConfig = CONFIG): V2ProviderTool[] {
  const added: V2ProviderTool[] = [];
  registerAftTools(
    {
      tool: {
        transform(register) {
          register({ add: (definition) => added.push(definition), remove: () => {} });
        },
      },
    },
    { directory: "/work/project" },
    definitions(config),
  );
  return added;
}

describe("OpenCode 2 tool name map", () => {
  test("bash and apply_patch take the host's names; every other tool keeps its own", () => {
    expect(V2_HOST_TOOL_NAMES).toEqual({ bash: "shell", apply_patch: "patch" });
    expect(v2HostToolName("bash")).toBe("shell");
    expect(v2HostToolName("apply_patch")).toBe("patch");
    for (const name of ["read", "edit", "write", "bash_status", "bash_watch", "aft_search"]) {
      expect(v2HostToolName(name)).toBe(name);
    }
    expect(aftToolNameForV2Host("shell")).toBe("bash");
    expect(aftToolNameForV2Host("patch")).toBe("apply_patch");
    expect(aftToolNameForV2Host("bash_status")).toBe("bash_status");
  });

  test("text references to the tools are renamed and nothing else is", () => {
    const renamed: Array<[string, string]> = [
      ["or use bash({wait:true}) when", "or use shell({wait:true}) when"],
      ["spawned with bash({ background: true }).", "spawned with shell({ background: true })."],
      ["Use the `apply_patch` tool to edit files.", "Use the `patch` tool to edit files."],
      ["through `bash` to locate or read code", "through `shell` to locate or read code"],
      ["command-line utilities in bash. Always", "command-line utilities in shell. Always"],
      ["DO NOT use bash for code search", "DO NOT use shell for code search"],
      ["a background or PTY bash task's state", "a background or PTY shell task's state"],
      ["or use bash `rm -rf` when", "or use shell `rm -rf` when"],
    ];
    for (const [before, after] of renamed) expect(renameV2ToolReferences(before)).toBe(after);

    const kept = [
      "at most bash.worker_wait_max_ms, 30 minutes",
      "`bash.detach_on_user_message: false`",
      "check bash_status reports mode",
      "e.g. bash-6b454047a1c39ded.",
      "interactive programs (python/node/bash REPLs, vim)",
      "terminal apps (python, node, bash itself, vim)",
      "Rewrite it in bash syntax",
      "aft_apply_patch and apply_patch_failed",
    ];
    for (const text of kept) expect(renameV2ToolReferences(text)).toBe(text);
  });
});

describe("OpenCode 2 registered surface", () => {
  test("registers shell and patch once each and never bash or apply_patch", () => {
    const names = projected().map((definition) => definition.name);
    expect(names.filter((name) => name === "shell")).toHaveLength(1);
    expect(names.filter((name) => name === "patch")).toHaveLength(1);
    expect(names).not.toContain("bash");
    expect(names).not.toContain("apply_patch");
    for (const companion of ["bash_status", "bash_watch", "bash_kill", "bash_write"]) {
      expect(names).toContain(companion);
    }
  });

  test("no description or argument description still points the model at bash or apply_patch", () => {
    const offenders: string[] = [];
    for (const definition of projected()) {
      const texts = [
        definition.description,
        JSON.stringify(tool.schema.toJSONSchema(definition.input, { io: "input" })),
      ];
      for (const text of texts) {
        if (renameV2ToolReferences(text) !== text) offenders.push(definition.name);
      }
    }
    expect(offenders).toEqual([]);
    const shell = projected().find((definition) => definition.name === "shell");
    expect(shell?.description).toStartWith("Execute shell commands.");
    expect(shell?.description).toContain("use shell({wait:true})");
    const patch = projected().find((definition) => definition.name === "patch");
    expect(patch?.description).toStartWith("Use the `patch` tool to edit files.");
    const status = projected().find((definition) => definition.name === "bash_status");
    const taskId = tool.schema.toJSONSchema(status?.input as never, { io: "input" }) as {
      properties: { taskId: { description: string } };
    };
    expect(taskId.properties.taskId.description).toContain(
      "returned by shell({ background: true })",
    );
  });

  test("the OpenCode 1 definitions are left exactly as they were", () => {
    const shared = definitions();
    const before = Object.fromEntries(
      Object.entries(shared).map(([name, definition]) => [
        name,
        {
          description: definition.description,
          schema: JSON.stringify(
            tool.schema.toJSONSchema(tool.schema.object(definition.args), { io: "input" }),
          ),
        },
      ]),
    );
    registerAftTools(
      {
        tool: {
          transform(register) {
            register({ add: () => {}, remove: () => {} });
          },
        },
      },
      { directory: "/work/project" },
      shared,
    );
    for (const [name, definition] of Object.entries(shared)) {
      expect(definition.description).toBe(before[name].description);
      expect(
        JSON.stringify(
          tool.schema.toJSONSchema(tool.schema.object(definition.args), { io: "input" }),
        ),
      ).toBe(before[name].schema);
    }
    expect(shared.bash.description).toContain("use bash({wait:true})");
  });

  test("the trimmed workflow hints name the registered shell tool once renamed", () => {
    const tools = definitions();
    const hints = buildHintsForRegisteredTools(CONFIG, new Set(Object.keys(tools)));
    if (hints === null) throw new Error("expected a hints block");
    const v2 = renameV2ToolReferences(hints);
    expect(v2).toContain("Don't find or read code with grep, find, sed or cat in shell.");
    expect(v2).toContain("Pass wait: true when you need a long command's result");
    expect(v2).not.toMatch(/\bbash\b/);
  });
});

type Hook = (event: any) => Effect.Effect<unknown, unknown>;

function hookHost() {
  const toolHooks = new Map<string, Hook>();
  const sessionHooks = new Map<string, Hook>();
  return {
    toolHooks,
    sessionHooks,
    context: {
      tool: {
        hook: (name: string, callback: Hook) =>
          Effect.sync(() => {
            toolHooks.set(name, callback);
          }),
      },
      session: {
        hook: (name: string, callback: Hook) =>
          Effect.sync(() => {
            sessionHooks.set(name, callback);
          }),
      },
    },
  };
}

async function registerHooks(registered: Iterable<string>, bashDescription?: string) {
  const host = hookHost();
  await Effect.runPromise(
    registerV2ToolHooks(host.context, {}, new Set(registered), bashDescription) as Effect.Effect<
      unknown,
      never
    >,
  );
  return host;
}

const HOST_SHELL_TEXT =
  "Execute a shell command and return its output. Commands run on macOS using zsh.";

function requestEvent(model: string, shellDescription: string) {
  return {
    sessionID: "session",
    model: { id: model, providerID: "openai" },
    tools: {
      shell: { description: shellDescription, input: {} },
      patch: { description: "patch", input: {} },
      edit: { description: "edit", input: {} },
      write: { description: "write", input: {} },
    },
    system: [],
    messages: [],
    options: {},
  };
}

describe("OpenCode 2 tool hooks follow the registered names", () => {
  test("argument preparation runs for shell and patch calls", async () => {
    const host = await registerHooks(["bash", "apply_patch"]);
    const before = host.toolHooks.get("execute.before");
    if (!before) throw new Error("no execute.before hook");
    for (const name of ["shell", "patch"]) {
      // A non-object input is refused by the preparation step, so a refusal
      // proves the hook recognised the host name as an AFT tool.
      await expect(
        Effect.runPromise(before({ tool: name, input: "not an object" })),
      ).rejects.toThrow("tool arguments must be an object");
    }
    // The host's own tools under names AFT never registered are left alone.
    const untouched = await registerHooks(["read"]);
    const event = { tool: "shell", input: "not an object" };
    await Effect.runPromise(untouched.toolHooks.get("execute.before")?.(event) ?? Effect.void);
    expect(event.input).toBe("not an object");
  });

  test("the merge-conflict hint is appended to a shell result", async () => {
    const host = await registerHooks(["bash", "aft_conflicts"]);
    const output =
      "Automatic merge failed; fix conflicts and then commit the result.\nCONFLICT (content): a";
    const event = { tool: "shell", status: "completed", result: { content: output } };
    await Effect.runPromise(host.toolHooks.get("execute.after")?.(event) ?? Effect.void);
    expect(event.result.content).not.toBe(output);
    expect(event.result.content).toStartWith(output);
  });

  test("the GPT editing gate removes patch, not apply_patch, for other models", async () => {
    const host = await registerHooks(["apply_patch", "edit", "write"]);
    const other = requestEvent("claude-sonnet", HOST_SHELL_TEXT);
    await Effect.runPromise(host.sessionHooks.get("context")?.(other) ?? Effect.void);
    expect(Object.keys(other.tools).sort()).toEqual(["edit", "shell", "write"]);
    const gpt = requestEvent("gpt-5", HOST_SHELL_TEXT);
    await Effect.runPromise(host.sessionHooks.get("generate")?.(gpt) ?? Effect.void);
    expect(Object.keys(gpt.tools).sort()).toEqual(["patch", "shell"]);
  });

  test("AFT's shell description is restored after the host's built-in rewrote it", async () => {
    const aftDescription = definitions().bash.description;
    const expected = renameV2ToolReferences(aftDescription);
    const host = await registerHooks(["bash"], aftDescription);
    for (const name of ["context", "compaction", "generate"]) {
      const event = requestEvent("gpt-5", HOST_SHELL_TEXT);
      await Effect.runPromise(host.sessionHooks.get(name)?.(event) ?? Effect.void);
      expect(event.tools.shell.description).toBe(expected);
    }
    // A shell tool some other plugin registered keeps its own description.
    const foreign = requestEvent("gpt-5", "Another plugin's shell.");
    await Effect.runPromise(host.sessionHooks.get("context")?.(foreign) ?? Effect.void);
    expect(foreign.tools.shell.description).toBe("Another plugin's shell.");
    // With AFT's bash not registered, the host's own shell is not touched.
    const unregistered = await registerHooks(["read"], aftDescription);
    const hostOnly = requestEvent("gpt-5", HOST_SHELL_TEXT);
    await Effect.runPromise(unregistered.sessionHooks.get("context")?.(hostOnly) ?? Effect.void);
    expect(hostOnly.tools.shell.description).toBe(HOST_SHELL_TEXT);
  });
});
