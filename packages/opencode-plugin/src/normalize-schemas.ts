import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";

type ToolArgSchema = ToolDefinition["args"][string];

type SchemaWithJsonSchemaOverride = ToolArgSchema & {
  _zod: ToolArgSchema["_zod"] & {
    toJSONSchema?: () => unknown;
  };
};

function stripRootJsonSchemaFields(jsonSchema: Record<string, unknown>): Record<string, unknown> {
  const { $schema: _schema, ...rest } = jsonSchema;
  return rest;
}

function attachJsonSchemaOverride(schema: SchemaWithJsonSchemaOverride): void {
  if (schema._zod.toJSONSchema) {
    return;
  }

  schema._zod.toJSONSchema = (): Record<string, unknown> => {
    const originalOverride = schema._zod.toJSONSchema;
    delete schema._zod.toJSONSchema;

    try {
      return stripRootJsonSchemaFields(tool.schema.toJSONSchema(schema));
    } finally {
      schema._zod.toJSONSchema = originalOverride;
    }
  };
}

/**
 * Patch tool arg schemas so that `.describe()` and `.meta()` survive
 * cross-Zod-instance JSON Schema serialization.
 *
 * OpenCode's host Zod can't see descriptions set by the plugin's Zod.
 * This patches `_zod.toJSONSchema` on each arg to use the plugin's own
 * `tool.schema.toJSONSchema`, which preserves all metadata.
 */
export function normalizeToolArgSchemas<T extends Pick<ToolDefinition, "args">>(
  toolDefinition: T,
): T {
  for (const schema of Object.values(toolDefinition.args)) {
    attachJsonSchemaOverride(schema);
  }
  return toolDefinition;
}

/**
 * Normalize all tool definitions in a record.
 * Applies `normalizeToolArgSchemas` to each tool in the map.
 */
export function normalizeToolMap(
  tools: Record<string, ToolDefinition>,
): Record<string, ToolDefinition> {
  for (const def of Object.values(tools)) {
    normalizeToolArgSchemas(def);
  }
  return tools;
}
