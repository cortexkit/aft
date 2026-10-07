import { expect, test } from "bun:test";
import { tool } from "@opencode-ai/plugin";
import {
  registerPiToolSurface,
  resolvePiToolSurface,
} from "../../../pi-plugin/src/tool-registration.js";
import type { AftConfig } from "../config.js";
import {
  buildAftToolDefinitions,
  registerAftTools,
  type V2ToolEditor,
} from "../tool-registration.js";
import type { V2ProviderTool } from "../tools/definitions/v2.js";
import type { PluginContext } from "../types.js";

function context(config: AftConfig): PluginContext {
  return {
    pool: {
      getBridge: () => {
        throw new Error("registration must not start a bridge");
      },
    },
    client: { lsp: {}, find: {} },
    config,
    storageDir: "/isolated/registration",
  } as unknown as PluginContext;
}

function profile(runon: boolean, reduced: boolean): AftConfig {
  return {
    disabled_tools: reduced
      ? ["aft_search", "aft_zoom", "aft_outline", "bash_watch", "bash_status", "bash_write"]
      : [],
    subc: { connection_file: "/configured/daemon.json" },
    remote_exec: { enabled: true },
    bash: { runon_enabled: runon },
  } as AftConfig;
}

for (const reduced of [false, true]) {
  for (const enabled of [false, true]) {
    test(`real OpenCode 1, OpenCode 2 and Pi registration never throws (reduced=${reduced}, runon=${enabled})`, () => {
      const config = profile(enabled, reduced);
      const ctx = context(config);
      const definitions = buildAftToolDefinitions(ctx, config);
      expect(definitions.bash).toBeDefined();
      expect("runon" in definitions.bash.args).toBe(enabled);
      expect(definitions.bash.description.includes("When remote runs are available")).toBe(enabled);
      if (reduced) {
        for (const companion of [
          "aft_search",
          "aft_zoom",
          "aft_outline",
          "bash_watch",
          "bash_status",
          "bash_write",
        ]) {
          expect(definitions.bash.description).not.toContain(companion);
        }
      }
      const projected = new Map<string, V2ProviderTool>();
      registerAftTools(
        {
          tool: {
            transform(callback) {
              callback({
                add: (definition) => projected.set(definition.name, definition),
                remove: () => {},
              });
            },
          },
        },
        { directory: "/configured/project" },
        definitions,
      );
      const bash = projected.get("bash")!;
      const schema = tool.schema.toJSONSchema(bash.input as never, { io: "input" }) as {
        properties: Record<string, unknown>;
      };
      expect("runon" in schema.properties).toBe(enabled);
      expect(bash.description?.includes("When remote runs are available")).toBe(enabled);

      const piDefinitions = new Map<
        string,
        { name: string; description: string; parameters: { properties: Record<string, unknown> } }
      >();
      const pi = {
        registerTool(definition: {
          name: string;
          description: string;
          parameters: { properties: Record<string, unknown> };
        }) {
          piDefinitions.set(definition.name, definition);
        },
      };
      registerPiToolSurface(pi as never, ctx as never, resolvePiToolSurface(config as never), "pi");
      const piBash = piDefinitions.get("bash")!;
      expect("runon" in piBash.parameters.properties).toBe(enabled);
      expect(piBash.description.includes("When remote runs are available")).toBe(enabled);
    });
  }
}

test("real OpenCode registration preserves narrowed wording and reprojects the live runon gate", () => {
  const config = profile(false, true);
  const ctx = context(config);
  const definitions = buildAftToolDefinitions(ctx, config);
  let transform: ((editor: V2ToolEditor) => void) | undefined;
  registerAftTools(
    {
      tool: {
        transform(callback) {
          transform = callback;
        },
      },
    },
    { directory: "/configured/project" },
    definitions,
  );
  for (const enabled of [true, false]) {
    ctx.config = { ...ctx.config, bash: { runon_enabled: enabled } };
    const projected = new Map<string, V2ProviderTool>();
    transform!({
      add: (definition) => projected.set(definition.name, definition),
      remove: () => {},
    });
    const bash = projected.get("bash")!;
    const schema = tool.schema.toJSONSchema(bash.input as never, { io: "input" }) as {
      properties: Record<string, unknown>;
    };
    expect("runon" in schema.properties).toBe(enabled);
    expect(bash.description?.includes("When remote runs are available")).toBe(enabled);
    expect(bash.description).not.toContain("bash_watch");
    expect(bash.description).not.toContain("aft_zoom");
  }
});
