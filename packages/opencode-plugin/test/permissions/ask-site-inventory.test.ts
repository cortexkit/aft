import { describe, expect, test } from "bun:test";
import { readdir, readFile } from "node:fs/promises";
import { extname, relative, resolve } from "node:path";
import { tool } from "@opencode-ai/plugin";
import { Effect } from "effect";

import { registerAftTools, type V2ToolEditor } from "../../src/tool-registration.js";
import {
  V2_AFT_FILESYSTEM_TOOLS,
  V2_BUILTIN_REPLACEMENTS,
  V2_PERMISSION_ASK_INVENTORY,
} from "../../src/tools/hoisted/v2.js";
import { v2HostToolName } from "../../src/v2-host-tool-names.js";

const ROOT = resolve(import.meta.dir, "../..");
const ASK_SITE_FIXTURE = {
  "src/tools/bash.ts": ["runtime.ask(", "context.ask("],
  "src/tools/hoisted.ts": ["context.ask(", "context.ask(", "context.ask("],
  "src/tools/permissions.ts": [
    "context.ask(",
    "context.ask(",
    "context.ask(",
    "context.ask(",
    "context.ask(",
  ],
  "src/tools/outside-permissions.ts": ["runtime.ask("],
} as const;

function askSites(source: string): string[] {
  return [...source.matchAll(/\b(?:context|runtime)\.ask\s*\(/g)].map((match) =>
    match[0].replace(/\s+/g, ""),
  );
}

async function typescriptFiles(directory: string): Promise<string[]> {
  const entries = await readdir(directory, { withFileTypes: true });
  const nested = await Promise.all(
    entries.map((entry) => {
      const path = resolve(directory, entry.name);
      if (entry.isDirectory()) return typescriptFiles(path);
      return Promise.resolve(extname(entry.name) === ".ts" ? [path] : []);
    }),
  );
  return nested.flat();
}

describe("OpenCode V2 permission inventory", () => {
  test("keeps the closed logical ask inventory fixture", () => {
    expect(V2_PERMISSION_ASK_INVENTORY).toEqual([
      "read",
      "edit",
      "write",
      "apply_patch",
      "aft_delete",
      "aft_move",
      "bash:withPermissionLoop",
      "bash:host-fallback",
    ]);
  });

  test("fails when a shared V2 path gains an unreviewed context.ask call site", async () => {
    const actual: Record<string, string[]> = {};
    for (const file of await typescriptFiles(resolve(ROOT, "src"))) {
      const sites = askSites(await readFile(file, "utf8"));
      if (sites.length > 0) actual[relative(ROOT, file)] = sites;
    }
    expect(actual).toEqual(ASK_SITE_FIXTURE);
  });

  test("removes only replaced built-ins and never AFT-prefixed filesystem tools", async () => {
    const added: Array<[namespace: unknown, name: string, codemode: boolean]> = [];
    const removed: string[] = [];
    const definition = {
      description: "fixture",
      args: { value: tool.schema.string() },
      execute: async () => "ok",
    };
    const definitions = Object.fromEntries(
      [...V2_BUILTIN_REPLACEMENTS, ...V2_AFT_FILESYSTEM_TOOLS].map((name) => [name, definition]),
    );
    const context = {
      tool: {
        transform: (transform: (editor: V2ToolEditor) => void) =>
          Effect.sync(() =>
            transform({
              add: (registered) =>
                added.push([
                  registered.options.namespace,
                  registered.name,
                  registered.options.codemode,
                ]),
              remove: (name) => removed.push(name),
            }),
          ),
      },
    };

    await Effect.runPromise(
      registerAftTools(context, { directory: "/work/project" }, definitions) as Effect.Effect<void>,
    );

    // The removal and the registration use the OpenCode 2 name, which for
    // apply_patch is `patch`.
    expect(removed).toEqual(V2_BUILTIN_REPLACEMENTS.map(v2HostToolName));
    expect(removed).not.toContain("aft_delete");
    expect(removed).not.toContain("aft_move");
    expect(added).toEqual(
      [...V2_BUILTIN_REPLACEMENTS, ...V2_AFT_FILESYSTEM_TOOLS].flatMap((name) => {
        const direct = [undefined, v2HostToolName(name), false];
        return name === "read" ? [direct, ["aft.code", "read", true]] : [direct];
      }),
    );
  });
});
