/// <reference path="../bun-test.d.ts" />
/**
 * Every filesystem mutation must name the file it touches the same way.
 *
 * A user's permission rule is matched against the resource string a tool
 * states when it asks. When write and edit stated `src/app.ts` while delete
 * and move stated `/Users/.../project/src/app.ts`, a rule such as
 * `{action: "edit", resource: "src/*"}` covered part of the class the user
 * thought it covered and silently missed the rest.
 *
 * The rule evaluation below reuses `decidePermission`, which carries the
 * host's own wildcard matching, rather than a second copy of those semantics
 * written for this test.
 */
import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, realpath, rm } from "node:fs/promises";
import * as path from "node:path";
import type { BridgePool, ToolCallOptions } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";

import { decidePermission, type V2PermissionRule } from "../permissions/v2.js";
import { _resetSessionDirectoryCacheForTest } from "../shared/session-directory.js";
import { hoistedTools } from "../tools/hoisted.js";
import type { PluginContext } from "../types.js";

type AskCall = {
  permission?: string;
  patterns?: string[];
  always?: string[];
  metadata?: Record<string, unknown>;
};

type Tools = Record<string, ToolDefinition>;

/**
 * One filesystem mutation, run against `source` (plus `destination` for move).
 * `apply_patch` takes no path argument: it learns its files from the server's
 * preview, which the harness answers with `source`.
 *
 * `inProjectResources` is what the tool must state for `src/app.ts` (and
 * `src/moved.ts`) inside the project; `outsideResources` is what it must state
 * for the same operation on paths outside the project root.
 */
type MutationCase = {
  tool: string;
  run: (
    tools: Tools,
    context: ToolContext,
    source: string,
    destination: string,
  ) => Promise<unknown>;
  inProjectResources: string[];
  outsideResources: (source: string, destination: string) => string[];
};

const WRITE_CASE: MutationCase = {
  tool: "write",
  run: (tools, context, source) =>
    tools.write.execute({ filePath: source, content: "export const ok = true;\n" }, context),
  inProjectResources: ["src/app.ts"],
  outsideResources: (source) => [source],
};

const EDIT_CASE: MutationCase = {
  tool: "edit",
  run: (tools, context, source) =>
    tools.edit.execute({ filePath: source, oldString: "before", newString: "after" }, context),
  inProjectResources: ["src/app.ts"],
  outsideResources: (source) => [source],
};

const APPLY_PATCH_CASE: MutationCase = {
  tool: "apply_patch",
  run: (tools, context) =>
    tools.apply_patch.execute({ patchText: "*** Begin Patch\n*** End Patch" }, context),
  inProjectResources: ["src/app.ts"],
  outsideResources: (source) => [source],
};

const DELETE_CASE: MutationCase = {
  tool: "aft_delete",
  run: (tools, context, source) => tools.aft_delete.execute({ files: [source] }, context),
  inProjectResources: ["src/app.ts"],
  outsideResources: (source) => [source],
};

const MOVE_CASE: MutationCase = {
  tool: "aft_move",
  run: (tools, context, source, destination) =>
    tools.aft_move.execute({ filePath: source, destination }, context),
  inProjectResources: ["src/app.ts", "src/moved.ts"],
  outsideResources: (source, destination) => [source, destination],
};

const MUTATIONS: MutationCase[] = [WRITE_CASE, EDIT_CASE, APPLY_PATCH_CASE, DELETE_CASE, MOVE_CASE];

let tmpRoot: string | null = null;

afterEach(async () => {
  if (tmpRoot) {
    await rm(tmpRoot, { recursive: true, force: true });
    tmpRoot = null;
  }
  _resetSessionDirectoryCacheForTest();
});

/**
 * Project and external directories side by side, both real paths so the
 * containment checks compare canonicalized strings. They live under the
 * current working directory rather than the system temp root, which
 * permission checks exempt from the external-directory prompt.
 */
async function makeProjectAndExternalDirs(): Promise<{ project: string; external: string }> {
  tmpRoot = await realpath(await mkdtemp(path.join(process.cwd(), ".aft-permission-resource-")));
  const project = path.join(tmpRoot, "project");
  const external = path.join(tmpRoot, "external");
  await mkdir(path.join(project, "src"), { recursive: true });
  await mkdir(external, { recursive: true });
  return { project, external };
}

function createMockClient(): PluginContext["client"] {
  return {
    lsp: { status: async () => ({ data: [] }) },
    find: { symbols: async () => ({ data: [] }) },
  } as unknown as PluginContext["client"];
}

/**
 * Hoisted tools over a bridge that approves everything. `patchFile` is the
 * absolute path the apply_patch preview reports as affected, which is where
 * that tool's stated resource comes from.
 */
function createTools(patchFile: string): Tools {
  const bridge = {
    toolCall: async (
      _sessionID: string | undefined,
      name: string,
      params: Record<string, unknown> = {},
      options?: ToolCallOptions,
    ) => {
      if (options?.preview === true) {
        return {
          success: true,
          preview: true,
          preview_diff: "",
          affected_paths: [patchFile],
          affected_rel_paths: [path.basename(patchFile)],
          text: "preview ok",
        };
      }
      if (name === "delete") {
        const files = (params.files as string[]) ?? [];
        return {
          success: true,
          text: `Deleted ${files.length}/${files.length} file(s)`,
          complete: true,
          deleted: files.map((file) => ({ file, backup_id: null })),
          skipped_files: [],
        };
      }
      return { success: true, text: "ok" };
    },
  };
  const pool = { getBridge: () => bridge } as unknown as BridgePool;
  return hoistedTools({
    pool,
    client: createMockClient(),
    config: {} as PluginContext["config"],
    storageDir: path.join(tmpRoot ?? process.cwd(), ".storage"),
  } as PluginContext);
}

function createSdkContext(directory: string, ask: ToolContext["ask"]): ToolContext {
  return {
    sessionID: "permission-resource-parity-test",
    messageID: "message-id",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask,
  };
}

function recordingAsk(calls: AskCall[]): ToolContext["ask"] {
  return (async (input: AskCall) => {
    calls.push(input);
  }) as unknown as ToolContext["ask"];
}

/** Run one mutation and report the resources it stated for its edit ask. */
async function statedResources(
  mutation: MutationCase,
  project: string,
  source: string,
  destination: string,
): Promise<{ edit: string[]; asks: AskCall[] }> {
  const asks: AskCall[] = [];
  const patchFile = path.isAbsolute(source) ? source : path.join(project, source);
  const tools = createTools(patchFile);
  await mutation.run(tools, createSdkContext(project, recordingAsk(asks)), source, destination);
  const editAsk = asks.find((call) => call.permission === "edit");
  expect(editAsk).toBeDefined();
  return { edit: editAsk?.patterns ?? [], asks };
}

/** How a ruleset answers a mutation that stated these resources. */
function ruleEffect(resources: string[], rules: V2PermissionRule[]): string {
  return decidePermission(
    { permission: "edit", patterns: resources, always: ["*"], metadata: {} },
    rules,
  ).effect;
}

describe("filesystem mutations state one permission resource shape", () => {
  for (const mutation of MUTATIONS) {
    test(`${mutation.tool} states a project-relative resource for a file in the project`, async () => {
      const { project } = await makeProjectAndExternalDirs();

      const { edit } = await statedResources(mutation, project, "src/app.ts", "src/moved.ts");

      expect(edit).toEqual(mutation.inProjectResources);
    });

    test(`${mutation.tool} keeps the absolute resource for a path outside the project`, async () => {
      const { project, external } = await makeProjectAndExternalDirs();
      const source = path.join(external, "app.ts");
      const destination = path.join(external, "moved.ts");

      const { edit, asks } = await statedResources(mutation, project, source, destination);

      // An outside path has no project-relative form, so it must stay
      // absolute — and the external-directory permission that guards it has to
      // keep firing.
      expect(edit).toEqual(mutation.outsideResources(source, destination));
      expect(asks.some((call) => call.permission === "external_directory")).toBe(true);
    });
  }

  test("one edit rule scoped to src/* covers write, edit, apply_patch, delete, and move", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const rules: V2PermissionRule[] = [{ action: "edit", resource: "src/*", effect: "allow" }];
    const effects: Record<string, string> = {};

    for (const mutation of MUTATIONS) {
      const { edit } = await statedResources(mutation, project, "src/app.ts", "src/moved.ts");
      effects[mutation.tool] = ruleEffect(edit, rules);
    }

    expect(effects).toEqual({
      write: "allow",
      edit: "allow",
      apply_patch: "allow",
      aft_delete: "allow",
      aft_move: "allow",
    });
  });

  test("the same src/* rule still leaves an outside-project delete to be asked", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const rules: V2PermissionRule[] = [{ action: "edit", resource: "src/*", effect: "allow" }];
    const outside = path.join(external, "app.ts");

    const { edit } = await statedResources(DELETE_CASE, project, outside, outside);

    // Guards the test above: the shared rule allows because the resource is
    // stated relative to the project, not because every resource happens to
    // match `src/*`.
    expect(ruleEffect(edit, rules)).toBe("ask");
  });
});
