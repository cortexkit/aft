import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import type {
  ExtensionAPI,
  ToolDefinition as PiToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { tool } from "@opencode-ai/plugin";
import type { AftConfig as PiConfig } from "../../../../packages/pi-plugin/src/config.js";
import {
  registerPiToolSurface,
  resolvePiToolSurface,
} from "../../../../packages/pi-plugin/src/tool-registration.js";
import type { PluginContext as PiContext } from "../../../../packages/pi-plugin/src/types.js";
import type { AftConfig as OpenCodeConfig } from "../config.js";
import { buildSubcToolSchemas, SUBC_BARE_TOOL_NAMES } from "../subc-tool-schemas.js";
import {
  buildOpenCodeToolMap,
  openCodeEditSlotSurvives,
  openCodeHashlineDowngrade,
  openCodeHashlineEditRegistered,
  openCodeHashlineEffective,
  openCodeReadSlotSurvives,
} from "../tool-registration.js";
import type { PluginContext as OpenCodeContext } from "../types.js";

type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };
type JsonObject = { [key: string]: JsonValue };

type Profile = {
  id: string;
  harness: "opencode" | "pi";
  surface: "minimal" | "recommended" | "all";
};

type Manifest = {
  canonical_path_property_inventory: {
    rows: Array<{
      harnesses: string[];
      subc_artifact: string;
      tool: string;
      json_pointer: string;
      canonical: string;
      legacy: string;
      requiredness: string;
    }>;
  };
  subc_capability_inventory: {
    bare_tools: Array<{ name: string }>;
  };
  registration_profile_manifest: {
    profiles: Profile[];
    checked_expected_sets: Record<string, string[]>;
    host_only_allowlist: Array<{ harness: string; tool: string }>;
  };
  shared_tool_inventory: {
    tools: Array<{ opencode: string; pi: string }>;
    host_control_tools: Array<{ name: string }>;
  };
};

const manifest = JSON.parse(
  readFileSync(
    new URL("../../../../docs/v0.49-unified-tool-surface-inventory.json", import.meta.url),
    "utf8",
  ),
) as Manifest;

const profileConfigs: Record<Profile["id"], Record<string, unknown>> = {
  "REG-V049-OC-MIN": { tool_surface: "minimal", backup: { enabled: true }, bash: false },
  "REG-V049-OC-REC": {
    tool_surface: "recommended",
    hoist_builtin_tools: true,
    backup: { enabled: true },
    bash: true,
    search_index: true,
    semantic_search: true,
  },
  "REG-V049-OC-ALL": {
    tool_surface: "all",
    hoist_builtin_tools: true,
    backup: { enabled: true },
    bash: true,
    search_index: true,
    semantic_search: true,
  },
  "REG-V049-PI-MIN": { tool_surface: "minimal", backup: { enabled: true }, bash: false },
  "REG-V049-PI-REC": {
    tool_surface: "recommended",
    backup: { enabled: true },
    bash: true,
    search_index: true,
    semantic_search: true,
  },
  "REG-V049-PI-ALL": {
    tool_surface: "all",
    backup: { enabled: true },
    bash: true,
    search_index: true,
    semantic_search: true,
  },
};

function stubContext(config: Record<string, unknown>): OpenCodeContext & PiContext {
  const pool = {
    getBridge: () => {
      throw new Error("surface construction must not touch the bridge");
    },
  };
  return { pool, config, storageDir: "/tmp/aft-registration-parity" } as never;
}

function capturePiTools(config: Record<string, unknown>): Map<string, PiToolDefinition> {
  const tools = new Map<string, PiToolDefinition>();
  const pi = {
    registerTool(definition: PiToolDefinition) {
      tools.set(definition.name, definition);
    },
  } as unknown as ExtensionAPI;
  const ctx = stubContext(config) as PiContext;
  registerPiToolSurface(pi, ctx, resolvePiToolSurface(ctx.config as PiConfig));
  return tools;
}

function openCodeTools(config: Record<string, unknown>): Record<string, unknown> {
  return buildOpenCodeToolMap(stubContext(config) as OpenCodeContext, config as OpenCodeConfig);
}

function sorted(values: Iterable<string>): string[] {
  return [...values].sort();
}

function exceptionNames(harness: "opencode" | "pi"): Set<string> {
  const names = new Set<string>();
  for (const entry of manifest.registration_profile_manifest.host_only_allowlist) {
    if (entry.harness !== harness) continue;
    for (const name of entry.tool.split("/")) names.add(name);
  }
  return names;
}

function filteredNames(names: Iterable<string>, harness: "opencode" | "pi"): string[] {
  const exceptions = exceptionNames(harness);
  return sorted([...names].filter((name) => !exceptions.has(name)));
}

function schemaForOpenCode(definition: { args: Record<string, unknown> }): JsonObject {
  return tool.schema.toJSONSchema(tool.schema.object(definition.args), {
    io: "input",
  }) as JsonObject;
}

function schemaForPi(definition: { parameters?: unknown }): JsonObject {
  return (definition.parameters ?? {}) as JsonObject;
}

function aliasForTool(toolName: string, key: string): string {
  if (key === "filePath") return "path";
  if (key === "toFile") return "toPath";
  if (toolName === "bash_status" && key === "task_id") return "taskId";
  if (toolName === "bash_status" && key === "output_mode") return "outputMode";
  if (toolName === "bash_write" && key === "task_id") return "taskId";
  if (toolName === "bash_watch" && key === "task_id") return "taskId";
  if (toolName === "bash_watch" && key === "timeout_ms") return "timeoutMs";
  if (toolName === "bash_watch" && key === "output_mode") return "outputMode";
  if (toolName === "bash_kill" && key === "task_id") return "taskId";
  return key;
}

function canonicalizeSchema(toolName: string, value: unknown): JsonValue {
  if (Array.isArray(value)) {
    const items = value.map((item) => canonicalizeSchema(toolName, item));
    const unique = new Map(items.map((item) => [JSON.stringify(item), item]));
    const values = [...unique.values()];
    const numeric = values.find(
      (item) =>
        typeof item === "object" &&
        item !== null &&
        !Array.isArray(item) &&
        (item as JsonObject).type === "integer",
    );
    if (
      numeric &&
      values.some(
        (item) =>
          item !== numeric &&
          typeof item === "object" &&
          item !== null &&
          !Array.isArray(item) &&
          (item as JsonObject).type === "string",
      )
    ) {
      return [numeric];
    }
    return values.sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b)));
  }
  if (value === null || typeof value !== "object") {
    return value as JsonValue;
  }

  const source = value as Record<string, unknown>;
  const result: JsonObject = {};
  for (const key of Object.keys(source)) {
    if (
      key === "$schema" ||
      key === "description" ||
      key === "title" ||
      key === "default" ||
      (key === "additionalProperties" && source[key] === false)
    )
      continue;
    if (key === "type" && source[key] === "number") {
      result[key] = "integer";
      continue;
    }
    if (key === "anyOf" && Array.isArray(source[key])) {
      const branches = canonicalizeSchema(toolName, source[key]) as JsonValue[];
      if (
        branches.length === 1 &&
        typeof branches[0] === "object" &&
        branches[0] !== null &&
        !Array.isArray(branches[0])
      ) {
        Object.assign(result, branches[0]);
      } else {
        const constants = branches.map((branch) =>
          typeof branch === "object" && branch !== null && !Array.isArray(branch)
            ? (branch as JsonObject).const
            : undefined,
        );
        if (constants.every((constant) => typeof constant === "string")) {
          result.enum = sorted(constants as string[]);
          result.type = "string";
        } else {
          result.anyOf = branches;
        }
      }
      continue;
    }
    if (key === "exclusiveMinimum" && source[key] === 0) {
      result.minimum = 1;
      continue;
    }
    if (key === "properties" && source[key] && typeof source[key] === "object") {
      const properties: JsonObject = {};
      for (const [property, propertySchema] of Object.entries(
        source[key] as Record<string, unknown>,
      )) {
        const canonicalName = aliasForTool(toolName, property);
        const canonicalSchema = canonicalizeSchema(toolName, propertySchema);
        properties[canonicalName] ??= canonicalSchema;
      }
      result[key] = Object.fromEntries(
        Object.entries(properties).sort(([a], [b]) => a.localeCompare(b)),
      );
      continue;
    }
    if (key === "required" && Array.isArray(source[key])) {
      result[key] = sorted(
        (source[key] as unknown[]).map((item) => aliasForTool(toolName, String(item))),
      );
      continue;
    }
    result[key] = canonicalizeSchema(toolName, source[key]);
  }

  const properties = result.properties;
  if (properties && typeof properties === "object" && !Array.isArray(properties)) {
    const bare = toolName.startsWith("aft_") ? toolName.slice(4) : toolName;
    const required = new Set(Array.isArray(result.required) ? (result.required as string[]) : []);
    if (
      ["read", "write", "edit", "callgraph", "move", "import"].includes(bare) &&
      "path" in properties
    ) {
      required.add("path");
    }
    if (bare === "zoom" && "path" in properties && "symbol" in properties) {
      required.add("path");
    }
    if (required.size > 0) result.required = sorted(required);
  }
  return Object.fromEntries(
    Object.entries(result).sort(([a], [b]) => a.localeCompare(b)),
  ) as JsonObject;
}

function canonicalProjection(
  name: string,
  definition: { description?: string; args?: Record<string, unknown>; parameters?: unknown },
): string {
  // Host-specific field descriptions, wrappers, and compatibility aliases are
  // deliberately outside the shared projection. The top-level tool identity is
  // retained so a profile cannot pass by comparing only a schema intersection.
  const schema = definition.args
    ? schemaForOpenCode(definition as { args: Record<string, unknown> })
    : schemaForPi(definition);
  return JSON.stringify({
    name,
    description: name,
    schema: canonicalizeSchema(name, schema),
  });
}

function pointer(schema: JsonObject, pointerText: string): JsonObject | undefined {
  const segments = pointerText.split("/").slice(1);
  const visit = (current: unknown, index: number): JsonObject | undefined => {
    if (!current || typeof current !== "object" || Array.isArray(current)) return undefined;
    const object = current as JsonObject;
    const alternatives = object.anyOf;
    if (Array.isArray(alternatives)) {
      for (const alternative of alternatives) {
        const match = visit(alternative, index);
        if (match) return match;
      }
    }
    if (index >= segments.length) return object;
    if (segments[index] === "{index}") return visit(object.items, index + 1);
    const properties = object.properties;
    if (!properties || typeof properties !== "object" || Array.isArray(properties))
      return undefined;
    return visit((properties as JsonObject)[segments[index]], index + 1);
  };
  return visit(schema, 0);
}

describe("v0.49 production registration profiles", () => {
  for (const profile of manifest.registration_profile_manifest.profiles) {
    test(`${profile.id} invokes the production registration path`, () => {
      const config = profileConfigs[profile.id];
      const names =
        profile.harness === "opencode"
          ? Object.keys(openCodeTools(config))
          : [...capturePiTools(config).keys()];
      expect(sorted(names)).toEqual(
        manifest.registration_profile_manifest.checked_expected_sets[profile.id],
      );
    });
  }

  test("Pi default and explicit hoisting preserve the existing registration bytes", () => {
    const base = {
      tool_surface: "all",
      backup: { enabled: true },
      bash: true,
      search_index: true,
      semantic_search: true,
    };
    const snapshot = (config: Record<string, unknown>) =>
      JSON.stringify(
        [...capturePiTools(config)].map(([name, definition]) => ({
          name,
          label: definition.label,
          description: definition.description,
          parameters: definition.parameters,
        })),
      );

    expect(snapshot(base)).toBe(snapshot({ ...base, hoist_builtin_tools: true }));
  });

  test("Pi prefixed mode preserves native slots and keeps AFT wire names bare", () => {
    const tools = capturePiTools({
      tool_surface: "recommended",
      backup: { enabled: true },
      bash: true,
      search_index: true,
      hoist_builtin_tools: false,
    });
    const names = new Set(tools.keys());

    for (const name of [
      "aft_read",
      "aft_write",
      "aft_edit",
      "aft_grep",
      "aft_bash",
      "bash_status",
      "bash_watch",
      "bash_write",
      "bash_kill",
    ]) {
      expect(names.has(name), `${name} should register`).toBe(true);
    }
    for (const hostName of ["read", "write", "edit", "grep", "bash"]) {
      expect(names.has(hostName), `${hostName} should remain host-native`).toBe(false);
    }
  });

  test("full profiles are paired against the checked shared inventory", () => {
    const shared = sorted([
      ...manifest.shared_tool_inventory.tools.map((toolName) => toolName.opencode),
      ...manifest.shared_tool_inventory.host_control_tools.map((tool) => tool.name),
    ]);
    for (const profile of manifest.registration_profile_manifest.profiles.filter(
      (item) => item.surface === "all",
    )) {
      const config = profileConfigs[profile.id];
      const names =
        profile.harness === "opencode"
          ? Object.keys(openCodeTools(config))
          : [...capturePiTools(config).keys()];
      expect(filteredNames(names, profile.harness)).toEqual(shared);
    }
  });

  test("shared projections are byte-identical after host-wrapper canonicalization", () => {
    const opencode = openCodeTools(profileConfigs["REG-V049-OC-ALL"]) as Record<
      string,
      { description?: string; args?: Record<string, unknown> }
    >;
    const pi = capturePiTools(profileConfigs["REG-V049-PI-ALL"]);
    const hostOnly = exceptionNames("opencode");
    for (const inventoryTool of manifest.shared_tool_inventory.tools) {
      const name = inventoryTool.opencode;
      expect(hostOnly.has(name)).toBe(false);
      const piName = inventoryTool.pi;
      expect(canonicalProjection(name, opencode[name])).toBe(
        canonicalProjection(
          piName,
          pi.get(piName) as unknown as { description?: string; parameters?: unknown },
        ),
      );
    }
    for (const inventoryTool of manifest.shared_tool_inventory.host_control_tools) {
      const name = inventoryTool.name;
      expect(canonicalProjection(name, opencode[name])).toBe(
        canonicalProjection(
          name,
          pi.get(name) as unknown as { description?: string; parameters?: unknown },
        ),
      );
    }
  });
});

describe("v0.49 canonical path and subc inventories", () => {
  test("subc bare names are checked independently from host registration", () => {
    const expected = manifest.subc_capability_inventory.bare_tools.map((entry) => entry.name);
    expect(sorted(SUBC_BARE_TOOL_NAMES)).toEqual(sorted(expected));
    expect(Object.keys(buildSubcToolSchemas()).sort()).toEqual(sorted(expected));
  });

  test("canonical path rows exist at every checked schema depth", () => {
    const assertRows = (
      schemas: Record<string, JsonObject>,
      label: string,
      keyForRow: (row: Manifest["canonical_path_property_inventory"]["rows"][number]) => string,
    ): void => {
      for (const row of manifest.canonical_path_property_inventory.rows) {
        const schema = schemas[keyForRow(row)];
        expect(schema, `${label} ${row.tool} schema`).toBeDefined();
        const canonical = canonicalizeSchema(row.tool, schema) as JsonObject;
        const property = pointer(canonical, row.json_pointer);
        expect(property, `${label} ${row.tool}${row.json_pointer}`).toBeDefined();
        const parentPointer = row.json_pointer.slice(0, row.json_pointer.lastIndexOf("/")) || "/";
        const parent = pointer(canonical, parentPointer) ?? canonical;
        const properties = parent.properties as JsonObject | undefined;
        expect(
          properties?.[row.legacy],
          `${label} ${row.tool}${row.json_pointer} legacy`,
        ).toBeUndefined();
        expect(
          properties?.[row.canonical],
          `${label} ${row.tool}${row.json_pointer} canonical`,
        ).toBeDefined();
        if (row.requiredness === "required") {
          expect((parent.required as string[] | undefined) ?? []).toContain(row.canonical);
        }
      }
    };

    const subcSchemas = buildSubcToolSchemas();
    assertRows(subcSchemas, "subc", (row) => row.subc_artifact);

    const opencode = openCodeTools(profileConfigs["REG-V049-OC-ALL"]) as Record<
      string,
      { args: Record<string, unknown> }
    >;
    const opencodeSchemas = Object.fromEntries(
      Object.entries(opencode).map(([name, definition]) => [name, schemaForOpenCode(definition)]),
    );
    assertRows(opencodeSchemas, "OpenCode", (row) => row.tool);

    const pi = capturePiTools(profileConfigs["REG-V049-PI-ALL"]);
    const piSchemas = Object.fromEntries(
      [...pi.entries()].map(([name, definition]) => [name, schemaForPi(definition)]),
    );
    assertRows(piSchemas, "Pi", (row) => row.tool);
  });
});

describe("hashline edit schema selection", () => {
  test("both hosts expose exactly patch under the surviving edit slot", () => {
    const config = {
      tool_surface: "recommended",
      hoist_builtin_tools: true,
      edit_mode: "hashline",
    };

    const openCodeContext = stubContext(config) as OpenCodeContext;
    openCodeContext.hashlineEffective = true;
    const openCode = buildOpenCodeToolMap(openCodeContext, config as OpenCodeConfig) as Record<
      string,
      { args: Record<string, unknown> }
    >;
    const openCodeSchema = schemaForOpenCode(openCode.edit);
    expect(Object.keys(openCodeSchema.properties as JsonObject)).toEqual(["patch"]);
    expect(openCodeSchema.required).toEqual(["patch"]);

    const piContext = stubContext(config) as PiContext;
    piContext.hashlineEffective = true;
    const piTools = new Map<string, PiToolDefinition>();
    const pi = {
      registerTool(definition: PiToolDefinition) {
        piTools.set(definition.name, definition);
      },
    } as unknown as ExtensionAPI;
    registerPiToolSurface(pi, piContext, resolvePiToolSurface(config as PiConfig));
    const piSchema = schemaForPi(piTools.get("edit") as PiToolDefinition);
    expect(Object.keys(piSchema.properties as JsonObject)).toEqual(["patch"]);
    expect(piSchema.required).toEqual(["patch"]);
    expect(piSchema.additionalProperties).toBe(false);

    const governed = JSON.parse(
      readFileSync(
        new URL("../../../../crates/aft/src/hashline_edit_schemas.json", import.meta.url),
        "utf8",
      ),
    ) as { arms: { hashline: { schema: JsonObject } } };
    const governedSchema = canonicalizeSchema("edit", governed.arms.hashline.schema);
    expect(canonicalizeSchema("edit", openCodeSchema)).toEqual(governedSchema);
    expect(canonicalizeSchema("edit", piSchema)).toEqual(governedSchema);
  });

  test("a hashline session without the tagged read slot registers the legacy arm", () => {
    // The reported surface: AFT's `read` registration is gone, so OpenCode keeps
    // serving its own untagged read while `edit` survives. Nothing in that
    // session can mint a tag, so the patch arm must not be offered.
    const config = {
      tool_surface: "recommended",
      hoist_builtin_tools: true,
      edit_mode: "hashline",
      disabled_tools: ["read"],
    } as const;

    expect(openCodeReadSlotSurvives(config)).toBe(false);
    expect(openCodeHashlineEffective(config)).toBe(false);

    const ctx = stubContext(config) as OpenCodeContext;
    ctx.hashlineEffective = openCodeHashlineEffective(config);
    const tools = buildOpenCodeToolMap(ctx, config as OpenCodeConfig) as Record<
      string,
      { args: Record<string, unknown> }
    >;
    expect(Object.keys(tools)).not.toContain("read");
    const schema = schemaForOpenCode(tools.edit);
    expect(Object.keys(schema.properties as JsonObject)).toContain("filePath");
    expect(Object.keys(schema.properties as JsonObject)).not.toContain("patch");

    const registered = new Set(Object.keys(tools));
    expect(openCodeHashlineEditRegistered(config, registered)).toBe(false);
    expect(openCodeHashlineDowngrade(config, registered)).toEqual({
      code: "hashline_downgraded",
      reason: "tagged_read_unavailable",
    });
  });

  test("the default hashline surface keeps both slots and stays on the patch arm", () => {
    const config = {
      tool_surface: "recommended",
      hoist_builtin_tools: true,
      edit_mode: "hashline",
    } as const;

    const ctx = stubContext(config) as OpenCodeContext;
    ctx.hashlineEffective = openCodeHashlineEffective(config);
    const tools = buildOpenCodeToolMap(ctx, config as OpenCodeConfig) as Record<
      string,
      { args: Record<string, unknown> }
    >;
    const registered = new Set(Object.keys(tools));
    expect(registered.has("read")).toBe(true);
    expect(Object.keys(schemaForOpenCode(tools.edit).properties as JsonObject)).toEqual(["patch"]);
    expect(openCodeHashlineEditRegistered(config, registered)).toBe(true);
    expect(openCodeHashlineDowngrade(config, registered)).toBeNull();
  });

  test("a hashline session missing only the edit slot keeps the original reason", () => {
    const config = {
      tool_surface: "recommended",
      hoist_builtin_tools: true,
      edit_mode: "hashline",
      disabled_tools: ["edit"],
    } as const;
    const registered = new Set(["read"]);
    expect(openCodeHashlineDowngrade(config, registered)).toEqual({
      code: "hashline_downgraded",
      reason: "edit_not_registered",
    });
  });

  test("default schema stays legacy and final surface controls edit-slot eligibility", () => {
    const legacy = openCodeTools(profileConfigs["REG-V049-OC-REC"]) as Record<
      string,
      { args: Record<string, unknown> }
    >;
    expect(Object.keys(schemaForOpenCode(legacy.edit).properties as JsonObject)).toContain(
      "filePath",
    );
    expect(openCodeEditSlotSurvives({ tool_surface: "recommended" })).toBe(true);
    const hashlineConfig = {
      tool_surface: "recommended",
      hoist_builtin_tools: true,
      edit_mode: "hashline",
    } as const;
    expect(openCodeHashlineEffective(hashlineConfig)).toBe(true);
    expect(openCodeHashlineEffective({ ...hashlineConfig, disabled_tools: ["edit"] })).toBe(false);
    expect(openCodeEditSlotSurvives({ tool_surface: "minimal", edit_mode: "hashline" })).toBe(
      false,
    );
    expect(
      openCodeEditSlotSurvives({
        tool_surface: "recommended",
        edit_mode: "hashline",
        disabled_tools: ["edit"],
      }),
    ).toBe(false);
  });
});

describe("truncation contract description parity", () => {
  const TRUNCATION_SENTENCE =
    "When a list is cut, the reply ends with `shown N of M <unit> (<reason>) · narrow: <knobs>`; absence of that line means the list is complete.";

  test("truncation sentence is byte-identical across both plugins for every shared list tool", () => {
    const opencode = openCodeTools(profileConfigs["REG-V049-OC-ALL"]) as Record<
      string,
      { description?: string }
    >;
    const pi = capturePiTools(profileConfigs["REG-V049-PI-ALL"]);
    const sharedListTools = [
      "aft_callgraph",
      "aft_outline",
      "aft_search",
      "grep",
      "aft_inspect",
      "bash",
    ];

    for (const tool of sharedListTools) {
      const ocDesc = opencode[tool]?.description ?? "";
      const piDesc = (pi.get(tool) as { description?: string } | undefined)?.description ?? "";

      const ocSentence = ocDesc.includes(TRUNCATION_SENTENCE) ? TRUNCATION_SENTENCE : null;
      const piSentence = piDesc.includes(TRUNCATION_SENTENCE) ? TRUNCATION_SENTENCE : null;

      expect(ocSentence, `cross-plugin identity mismatch for ${tool}`).toBe(piSentence);
      expect(ocSentence, `missing truncation sentence in OpenCode ${tool}`).toBe(
        TRUNCATION_SENTENCE,
      );
      expect(piSentence, `missing truncation sentence in Pi ${tool}`).toBe(TRUNCATION_SENTENCE);
    }
  });

  test("truncation sentence appears exactly once per description of every registered surface's tool and in no other tool", () => {
    const opencode = openCodeTools(profileConfigs["REG-V049-OC-ALL"]) as Record<
      string,
      { description?: string }
    >;
    const pi = capturePiTools(profileConfigs["REG-V049-PI-ALL"]);

    const countOccurrences = (text: string, sub: string) => {
      let count = 0;
      let pos = 0;
      while ((pos = text.indexOf(sub, pos)) !== -1) {
        count++;
        pos += sub.length;
      }
      return count;
    };

    const ocListTools = new Set([
      "aft_callgraph",
      "aft_outline",
      "aft_search",
      "grep",
      "glob",
      "aft_inspect",
      "bash",
    ]);

    for (const [name, def] of Object.entries(opencode)) {
      const occurrences = countOccurrences(def.description ?? "", TRUNCATION_SENTENCE);
      if (ocListTools.has(name)) {
        expect(
          occurrences,
          `OpenCode tool ${name} must contain truncation sentence exactly once`,
        ).toBe(1);
      } else {
        expect(
          occurrences,
          `OpenCode non-list tool ${name} must not contain truncation sentence`,
        ).toBe(0);
      }
    }

    const piListTools = new Set([
      "aft_callgraph",
      "aft_outline",
      "aft_search",
      "grep",
      "aft_inspect",
      "bash",
    ]);

    for (const [name, def] of pi.entries()) {
      const occurrences = countOccurrences(def.description ?? "", TRUNCATION_SENTENCE);
      if (piListTools.has(name)) {
        expect(occurrences, `Pi tool ${name} must contain truncation sentence exactly once`).toBe(
          1,
        );
      } else {
        expect(occurrences, `Pi non-list tool ${name} must not contain truncation sentence`).toBe(
          0,
        );
      }
    }
  });
});
